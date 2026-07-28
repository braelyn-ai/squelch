// The "ask your inbox" agent loop. Runs entirely on this machine with the
// user's OWN key: build a /v1/messages body, fire it through LLMProxy, and walk
// the standard tool-use loop (search_mail / get_thread) until the model answers.
// Non-streaming — each turn is a short complete message we inspect for
// stop_reason; inbox answers are a paragraph, so there is nothing to stream.
//
// SECURITY POSTURE (mirrors the Tauri shell's `llm_complete` exactly): the key
// is read ONLY inside LLMProxy, from the keychain, at call time. It is never a
// parameter here, never a return value, never in an error, and never reachable
// from the view layer. This file cannot obtain it even if it wanted to —
// `AssistantKeyStore.read()` is fileprivate to Keychain.swift.
//
// The tools are thin wrappers over the existing human-door reads, so the
// assistant gets exactly the same sealed-absent, summary-first view the rest of
// the app does. Sealed (auth/2FA) messages are structurally absent from both
// routes, which is why the system prompt can promise it plainly.
//
// Ported from squelch-desktop/src/api/assistant/*.

import Foundation

// MARK: - wire shapes

/// Minimal Anthropic /v1/messages shapes — just the fields the loop reads. We
/// hand-roll these rather than pull an SDK because the HTTP call itself is made
/// by LLMProxy; this only builds the body and walks the response blocks.
private enum Wire {
    struct ToolDef: Encodable {
        var name: String
        var description: String
        var input_schema: Schema

        struct Schema: Encodable {
            var type = "object"
            var properties: [String: Property]
            var required: [String]?
        }
        struct Property: Encodable {
            var type: String
            var description: String
        }
    }

    /// A content block we SEND back (assistant echo or tool results).
    enum RequestBlock: Encodable {
        case text(String)
        case toolUse(id: String, name: String, input: [String: JSONValue])
        case toolResult(toolUseId: String, content: String, isError: Bool)

        func encode(to encoder: Encoder) throws {
            var c = encoder.container(keyedBy: CodingKeys.self)
            switch self {
            case .text(let text):
                try c.encode("text", forKey: .type)
                try c.encode(text, forKey: .text)
            case .toolUse(let id, let name, let input):
                try c.encode("tool_use", forKey: .type)
                try c.encode(id, forKey: .id)
                try c.encode(name, forKey: .name)
                try c.encode(input, forKey: .input)
            case .toolResult(let toolUseId, let content, let isError):
                try c.encode("tool_result", forKey: .type)
                try c.encode(toolUseId, forKey: .tool_use_id)
                try c.encode(content, forKey: .content)
                try c.encode(isError, forKey: .is_error)
            }
        }

        private enum CodingKeys: String, CodingKey {
            case type, text, id, name, input, tool_use_id, content, is_error
        }
    }

    struct MessageParam: Encodable {
        var role: String
        var content: Content

        enum Content: Encodable {
            case text(String)
            case blocks([RequestBlock])

            func encode(to encoder: Encoder) throws {
                var c = encoder.singleValueContainer()
                switch self {
                case .text(let s): try c.encode(s)
                case .blocks(let b): try c.encode(b)
                }
            }
        }
    }

    struct Request: Encodable {
        var model: String
        var max_tokens: Int
        var system: String
        var tools: [ToolDef]
        var messages: [MessageParam]
    }

    /// A content block we RECEIVE.
    struct ResponseBlock: Decodable {
        var type: String
        var text: String?
        var id: String?
        var name: String?
        var input: [String: JSONValue]?
    }

    struct Usage: Decodable {
        var input_tokens: Int?
        var output_tokens: Int?
    }

    struct Response: Decodable {
        var content: [ResponseBlock]?
        var stop_reason: String?
        var usage: Usage?
        var error: ProviderError?

        struct ProviderError: Decodable {
            var type: String?
            var message: String?
        }
    }
}

/// A minimal JSON value, so tool inputs can round-trip verbatim: a tool_use
/// block MUST be echoed back byte-identical or the provider rejects the turn.
enum JSONValue: Codable, Sendable {
    case string(String)
    case number(Double)
    case bool(Bool)
    case object([String: JSONValue])
    case array([JSONValue])
    case null

    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() {
            self = .null
        } else if let v = try? c.decode(Bool.self) {
            self = .bool(v)
        } else if let v = try? c.decode(Double.self) {
            self = .number(v)
        } else if let v = try? c.decode(String.self) {
            self = .string(v)
        } else if let v = try? c.decode([String: JSONValue].self) {
            self = .object(v)
        } else if let v = try? c.decode([JSONValue].self) {
            self = .array(v)
        } else {
            self = .null
        }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .string(let v): try c.encode(v)
        case .number(let v): try c.encode(v)
        case .bool(let v): try c.encode(v)
        case .object(let v): try c.encode(v)
        case .array(let v): try c.encode(v)
        case .null: try c.encodeNil()
        }
    }

    var stringValue: String? {
        if case .string(let s) = self { return s }
        return nil
    }
    var intValue: Int? {
        if case .number(let n) = self { return Int(n) }
        return nil
    }
}

// MARK: - citations

/// A source the assistant actually consulted — surfaced as an answer citation.
struct ToolCitation: Identifiable, Sendable, Hashable {
    var threadId: String
    var subject: String
    var sender: String
    var date: String
    var id: String { threadId }
}

struct AssistantAnswer: Sendable {
    var text: String
    var citations: [ToolCitation]
    var model: String
    var inputTokens: Int
    var outputTokens: Int
}

struct AssistantError: Error, LocalizedError {
    var message: String
    var errorDescription: String? { message }
}

// MARK: - the agent

@MainActor
enum Assistant {
    /// Safety valve on the tool loop (search + a few thread reads is plenty).
    private static let maxTurns = 6
    private static let maxTokens = 1024

    private static let system = """
        You are the user's personal inbox assistant, embedded in an app called squelch.
        Answer questions about their email using the tools — search_mail first to find
        relevant messages, then get_thread only when a snippet isn't enough.

        Rules:
        - Be concise and direct. Lead with the answer, then the supporting detail.
        - Ground every claim in what the tools returned. If you can't find it, say so
          plainly rather than guessing.
        - You are the user's stand-in: the whole point is that they never have to open
          the email themselves. Summarize; don't tell them to go read it.
        - Auth codes, 2FA, and password-reset messages are deliberately invisible to
          you. If asked for one, explain it's handled separately in the app, not here.
        - Dates in results are RFC3339; refer to them in plain language.
        """

    private static let tools: [Wire.ToolDef] = [
        Wire.ToolDef(
            name: "search_mail",
            description: """
                Search the user's mailbox by meaning AND keyword (hybrid recall). \
                Returns summaries only — sender, subject, date, a snippet, and a \
                thread_id — never full bodies. Auth/2FA messages are excluded. Call \
                this first to find the messages relevant to the question.
                """,
            input_schema: .init(
                properties: [
                    "query": .init(type: "string", description: "What to look for, phrased naturally."),
                    "limit": .init(type: "integer", description: "Max results to return (default 8, max 20)."),
                ],
                required: ["query"])),
        Wire.ToolDef(
            name: "get_thread",
            description: """
                Read one thread's messages by thread_id (from a search result) when \
                the snippet isn't enough to answer. Returns the subject and each \
                message's sender, date, and text.
                """,
            input_schema: .init(
                properties: [
                    "thread_id": .init(
                        type: "string", description: "The thread_id from a search_mail result.")
                ],
                required: ["thread_id"])),
    ]

    /// Ask a question and return a cited answer. Throws on a missing key, the
    /// wrong provider, a provider error, a refusal, or hitting the step limit.
    static func ask(_ question: String) async throws -> AssistantAnswer {
        let status = await AssistantKeyStore.statusAsync()
        guard status.present else {
            throw AssistantError(message: "No assistant key set — add one in Settings.")
        }
        guard status.provider == .anthropic else {
            throw AssistantError(
                message: "The assistant currently supports Anthropic keys (sk-ant-…). "
                    + "OpenAI support is coming; paste an Anthropic key in Settings for now.")
        }

        let model = Prefs.shared.assistantModel
        var messages: [Wire.MessageParam] = [
            .init(role: "user", content: .text(question.trimmed))
        ]

        // Threads surfaced by search vs. actually opened via get_thread. We cite
        // the opened ones when the model drilled in, else the top hits it saw.
        var cites: [String: ToolCitation] = [:]
        var citeOrder: [String] = []
        var readIds: [String] = []

        var inputTokens = 0
        var outputTokens = 0

        let encoder = JSONEncoder()
        let decoder = JSONDecoder()

        for _ in 0..<maxTurns {
            let request = Wire.Request(
                model: model.rawValue, max_tokens: maxTokens, system: system, tools: tools,
                messages: messages)
            let response = try await LLMProxy.complete(body: try encoder.encode(request))
            let parsed = try decoder.decode(Wire.Response.self, from: response.json)

            guard response.status == 200 else {
                throw AssistantError(
                    message: parsed.error?.message
                        ?? "assistant request failed (\(response.status))")
            }

            inputTokens += parsed.usage?.input_tokens ?? 0
            outputTokens += parsed.usage?.output_tokens ?? 0

            if parsed.stop_reason == "refusal" {
                throw AssistantError(message: "The model declined to answer that.")
            }

            let blocks = parsed.content ?? []

            // Echo the assistant turn VERBATIM — tool_use blocks must be
            // preserved exactly or the next turn is rejected.
            let echo: [Wire.RequestBlock] = blocks.compactMap { block in
                switch block.type {
                case "text": return .text(block.text ?? "")
                case "tool_use":
                    guard let id = block.id, let name = block.name else { return nil }
                    return .toolUse(id: id, name: name, input: block.input ?? [:])
                default: return nil
                }
            }
            messages.append(.init(role: "assistant", content: .blocks(echo)))

            guard parsed.stop_reason == "tool_use" else {
                let text =
                    blocks.filter { $0.type == "text" }.compactMap(\.text).joined()
                    .trimmingCharacters(in: .whitespacesAndNewlines)
                AssistantUsageLedger.record(
                    model: model, inputTokens: inputTokens, outputTokens: outputTokens)
                return AssistantAnswer(
                    text: text.isEmpty ? "(the assistant returned no text)" : text,
                    citations: pickCitations(cites, order: citeOrder, read: readIds),
                    model: model.rawValue, inputTokens: inputTokens, outputTokens: outputTokens)
            }

            // Run every requested tool, collecting all results into ONE user turn.
            var results: [Wire.RequestBlock] = []
            for block in blocks where block.type == "tool_use" {
                guard let id = block.id, let name = block.name else { continue }
                let input = block.input ?? [:]
                if name == "get_thread", let tid = input["thread_id"]?.stringValue, !tid.isEmpty {
                    readIds.append(tid)
                }
                let outcome = await runTool(name: name, input: input) { citation in
                    if cites[citation.threadId] == nil { citeOrder.append(citation.threadId) }
                    cites[citation.threadId] = citation
                }
                results.append(
                    .toolResult(
                        toolUseId: id, content: outcome.content, isError: outcome.isError))
            }
            messages.append(.init(role: "user", content: .blocks(results)))
        }

        throw AssistantError(message: "The assistant took too many steps without answering.")
    }

    private static func pickCitations(
        _ cites: [String: ToolCitation], order: [String], read: [String]
    ) -> [ToolCitation] {
        let opened = read.compactMap { cites[$0] }
        if !opened.isEmpty { return opened }
        // Nothing opened — cite the first few search hits the model actually saw.
        return Array(order.compactMap { cites[$0] }.prefix(5))
    }

    /// Execute one tool call. Errors are returned as `{ error }` JSON with
    /// is_error set, so the model can adapt rather than the loop throwing.
    private static func runTool(
        name: String, input: [String: JSONValue], cite: (ToolCitation) -> Void
    ) async -> (content: String, isError: Bool) {
        func json(_ object: Any) -> String {
            guard let data = try? JSONSerialization.data(withJSONObject: object),
                let string = String(data: data, encoding: .utf8)
            else { return "{}" }
            return string
        }

        do {
            switch name {
            case "search_mail":
                let query = (input["query"]?.stringValue ?? "").trimmed
                guard !query.isEmpty else {
                    return (json(["error": "empty query"]), true)
                }
                let limit = min(input["limit"]?.intValue ?? 8, 20)
                let page = try await APIClient.shared.search(query, limit: limit, mode: .hybrid)
                let rows = page.items.map { hit -> [String: Any] in
                    // Remember every hit as a candidate citation.
                    cite(
                        ToolCitation(
                            threadId: hit.thread_id,
                            subject: hit.subject.isEmpty ? "(no subject)" : hit.subject,
                            sender: hit.from_name ?? hit.from_addr, date: hit.received_at))
                    return [
                        "thread_id": hit.thread_id,
                        "from": hit.from_name ?? hit.from_addr,
                        "subject": hit.subject,
                        "date": hit.received_at,
                        "snippet": hit.snippet,
                    ]
                }
                return (json(["results": rows]), false)

            case "get_thread":
                let id = (input["thread_id"]?.stringValue ?? "").trimmed
                guard !id.isEmpty else {
                    return (json(["error": "missing thread_id"]), true)
                }
                let view = try await APIClient.shared.getThread(id)
                // A thread the model chose to OPEN is a strong citation signal.
                if let first = view.messages.first {
                    cite(
                        ToolCitation(
                            threadId: view.thread_id,
                            subject: view.subject.isEmpty ? "(no subject)" : view.subject,
                            sender: first.from_name ?? first.from_addr,
                            date: first.received_at))
                }
                let messages = view.messages.map { m -> [String: Any] in
                    [
                        "from": m.from_name ?? m.from_addr, "date": m.received_at,
                        "text": m.content,
                    ]
                }
                return (json(["subject": view.subject, "messages": messages]), false)

            default:
                return (json(["error": "unknown tool \(name)"]), true)
            }
        } catch {
            // Surface a compact error so the model can adapt (e.g. sealed → 404).
            return (json(["error": errText(error, "tool failed")]), true)
        }
    }
}

// MARK: - local usage ledger

/// Client-side token tally for the BYOK assistant. Its calls go straight from
/// this machine to the user's provider with their own key — the squelch server
/// never sees them, so this usage is tracked locally and feeds the Usage page's
/// "Assistant" slot. Entirely separate from server-side triage usage.
struct AssistantUsage: Sendable, Equatable {
    /// Completed asks (not per-turn API calls).
    var asks = 0
    var inputTokens = 0
    var outputTokens = 0
    var lastModel: String?
    var lastAt: String?

    /// Rough local cost estimate from the model's published rates.
    var estimatedCost: Double {
        let rates = AssistantModel(rawValue: lastModel ?? "")?.rates
            ?? AssistantModel.haiku.rates
        return Double(inputTokens) / 1_000_000 * rates.input
            + Double(outputTokens) / 1_000_000 * rates.output
    }
}

@MainActor
enum AssistantUsageLedger {
    private static let key = "squelch.assistant.usage"

    static func read() -> AssistantUsage {
        let d = UserDefaults.standard
        guard let dict = d.dictionary(forKey: key) else { return AssistantUsage() }
        return AssistantUsage(
            asks: dict["asks"] as? Int ?? 0,
            inputTokens: dict["inputTokens"] as? Int ?? 0,
            outputTokens: dict["outputTokens"] as? Int ?? 0,
            lastModel: dict["lastModel"] as? String,
            lastAt: dict["lastAt"] as? String)
    }

    /// Fold one completed ask (summed across its tool-loop turns) into the tally.
    static func record(model: AssistantModel, inputTokens: Int, outputTokens: Int) {
        let current = read()
        let next: [String: Any] = [
            "asks": current.asks + 1,
            "inputTokens": current.inputTokens + inputTokens,
            "outputTokens": current.outputTokens + outputTokens,
            "lastModel": model.rawValue,
            "lastAt": ISO8601DateFormatter().string(from: Date()),
        ]
        UserDefaults.standard.set(next, forKey: key)
    }
}
