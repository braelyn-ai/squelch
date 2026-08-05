// THE DECODER HALF OF THE STREAMING ASSISTANT: Anthropic's /v1/messages SSE
// protocol turned into typed events, one `data:` payload at a time. Pure and
// synchronous on purpose — LLMProxy owns the key, the HTTP call, and the SSE
// framing; this only reads frames, so the whole wire protocol is testable
// without a network (or a key) in the room.
//
// Hand-rolled rather than an SDK for the same reason Assistant.swift's Wire is:
// the call is already made by the time a payload reaches here.
//
// The tool_use contract is why the accumulator exists. A tool call arrives as a
// run of `input_json_delta` fragments that concatenate into one JSON object,
// and the block it belongs to must be echoed back to the provider BYTE-
// IDENTICAL or the next turn is rejected. So the fragments are joined and
// parsed into `JSONValue` (Assistant.swift's round-trip type), never into
// hand-rolled structs that would drop unknown fields on the way back out.

import Foundation

// MARK: - events

/// One decoded thing that happened on the wire. Deliberately flat: the session
/// above decides what a `textDelta` means for the UI, what a `toolUseComplete`
/// means for the tool loop, and whether a `providerError` is fatal.
enum AnthropicStreamEvent: Sendable {
    /// `message_start` — carries the prompt's token count for the ledger.
    case messageStart(inputTokens: Int)
    case textDelta(index: Int, text: String)
    /// A tool_use block opened. Its input is still arriving.
    case toolUseStarted(index: Int, id: String, name: String)
    /// A tool_use block closed and its accumulated input parsed.
    case toolUseComplete(index: Int, id: String, name: String, input: [String: JSONValue])
    case blockStop(index: Int)
    /// `message_delta` — `outputTokens` is CUMULATIVE for the whole message,
    /// not a delta, so callers assign rather than add.
    case messageDelta(stopReason: String?, outputTokens: Int?)
    case messageStop
    /// The provider sent an `error` event, or its tool input didn't parse.
    case providerError(type: String?, message: String)
}

// MARK: - the accumulator

/// Fed one SSE `data:` payload at a time, returns zero or more events. A value
/// type holding the only state the protocol requires: per-index block identity
/// and the partial tool-input JSON that hasn't closed yet.
///
/// Tolerant by construction, mirroring SSEParser: an undecodable payload, an
/// unknown event type, and a delta for a block we never saw a start for are all
/// SKIPPED rather than thrown. Unknown types especially — the provider adds
/// event types over time, and a client that errors on one it doesn't recognize
/// breaks on a day nobody deployed anything.
struct AnthropicStreamAccumulator {
    /// What we know about one open content block.
    private struct OpenBlock {
        var tool: (id: String, name: String)?
        /// Concatenated `input_json_delta` fragments; empty for text blocks.
        var partialJSON = ""
    }

    private var blocks: [Int: OpenBlock] = [:]

    private static let decoder = JSONDecoder()

    init() {}

    /// Decode one frame payload.
    mutating func feed(_ payload: String) -> [AnthropicStreamEvent] {
        guard let data = payload.data(using: .utf8),
            let frame = try? Self.decoder.decode(StreamWire.Frame.self, from: data)
        else { return [] }

        switch frame.type {
        case "message_start":
            return [.messageStart(inputTokens: frame.message?.usage?.input_tokens ?? 0)]

        case "content_block_start":
            guard let index = frame.index else { return [] }
            let block = frame.content_block
            if block?.type == "tool_use", let id = block?.id, let name = block?.name {
                blocks[index] = OpenBlock(tool: (id: id, name: name))
                return [.toolUseStarted(index: index, id: id, name: name)]
            }
            // Text (and anything new) needs no start event — the deltas say
            // everything the caller acts on.
            blocks[index] = OpenBlock()
            return []

        case "content_block_delta":
            guard let index = frame.index, let delta = frame.delta else { return [] }
            switch delta.type {
            case "text_delta":
                return [.textDelta(index: index, text: delta.text ?? "")]
            case "input_json_delta":
                guard let fragment = delta.partial_json, blocks[index] != nil else { return [] }
                blocks[index]?.partialJSON += fragment
                return []
            default:
                return []
            }

        case "content_block_stop":
            guard let index = frame.index else { return [] }
            let block = blocks.removeValue(forKey: index)
            guard let tool = block?.tool else { return [.blockStop(index: index)] }

            let raw = block?.partialJSON ?? ""
            guard let input = Self.parseToolInput(raw) else {
                // Never crash on a malformed input: hand the caller an empty
                // one so the tool_use block still round-trips, and say what
                // happened alongside it.
                return [
                    .toolUseComplete(index: index, id: tool.id, name: tool.name, input: [:]),
                    .providerError(
                        type: "malformed_tool_input",
                        message: "The assistant sent unreadable arguments for \(tool.name)."),
                    .blockStop(index: index),
                ]
            }
            return [
                .toolUseComplete(index: index, id: tool.id, name: tool.name, input: input),
                .blockStop(index: index),
            ]

        case "message_delta":
            return [
                .messageDelta(
                    stopReason: frame.delta?.stop_reason,
                    outputTokens: frame.usage?.output_tokens)
            ]

        case "message_stop":
            return [.messageStop]

        case "error":
            return [
                .providerError(
                    type: frame.error?.type,
                    message: frame.error?.message ?? "The assistant provider reported an error.")
            ]

        default:
            // ping, and every event type shipped after this was written.
            return []
        }
    }

    /// Parse the concatenated `partial_json` into the round-trippable shape.
    /// An empty accumulation is `{}` — that is what a no-argument tool call
    /// looks like on this wire, not a failure. nil means it truly didn't parse.
    private static func parseToolInput(_ raw: String) -> [String: JSONValue]? {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty { return [:] }
        guard let data = trimmed.data(using: .utf8),
            let input = try? decoder.decode([String: JSONValue].self, from: data)
        else { return nil }
        return input
    }
}

// MARK: - wire shapes

/// One SSE payload, flattened: every event type's fields on one optional-heavy
/// struct so a single decode handles all of them, and a field a given type
/// doesn't carry is simply nil.
private enum StreamWire {
    struct Frame: Decodable {
        var type: String
        var index: Int?
        var message: Message?
        var content_block: ContentBlock?
        var delta: Delta?
        var usage: Usage?
        var error: ProviderError?
    }

    struct Message: Decodable {
        var usage: Usage?
    }

    struct ContentBlock: Decodable {
        var type: String?
        var id: String?
        var name: String?
    }

    struct Delta: Decodable {
        var type: String?
        var text: String?
        var partial_json: String?
        var stop_reason: String?
    }

    struct Usage: Decodable {
        var input_tokens: Int?
        var output_tokens: Int?
    }

    struct ProviderError: Decodable {
        var type: String?
        var message: String?
    }
}
