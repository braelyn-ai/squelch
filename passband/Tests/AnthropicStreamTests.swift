// The wire protocol, in a room with no network: recorded /v1/messages SSE
// payloads fed to AnthropicStreamAccumulator, whole event sequences asserted.
// This is the payoff of keeping the decoder pure and synchronous — the suite
// compiles with just JSONValue.swift and AnthropicStream.swift (see test.sh),
// no app, no key, no provider.
//
// The frames are transcribed from real Anthropic streams, then edited down and
// made hostile: inputs split mid-escape, blocks interleaved, stops out of
// order, payloads that aren't JSON at all. Tolerances are asserted as hard as
// contracts — an accumulator that starts THROWING on unknown event types is as
// broken as one that drops a tool call.

import Foundation

@main
@MainActor
struct AnthropicStreamTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        plainTextMessage()
        fragmentedToolInput()
        noArgumentToolCall()
        malformedToolInput()
        garbageAndUnknownFrames()
        deltaForUnopenedBlock()
        interleavedBlocks()
        errorEvent()
        messageDeltaWithoutUsage()
        inputSurvivesReencoding()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    // MARK: - cases

    /// The simplest complete message: text in, text out, in order.
    static func plainTextMessage() {
        var acc = AnthropicStreamAccumulator()
        equal(
            acc.feed(
                #"{"type":"message_start","message":{"id":"msg_01X","type":"message","role":"assistant","model":"claude-haiku-4-5","content":[],"stop_reason":null,"usage":{"input_tokens":2095,"output_tokens":3}}}"#
            ),
            [.messageStart(inputTokens: 2095)], "message_start carries input tokens")
        equal(
            acc.feed(
                #"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#
            ),
            [], "text block start is silent")
        equal(
            acc.feed(
                #"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#
            ),
            [.textDelta(index: 0, text: "Hel")], "first delta")
        equal(
            acc.feed(
                #"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo."}}"#
            ),
            [.textDelta(index: 0, text: "lo.")], "second delta")
        equal(
            acc.feed(#"{"type":"content_block_stop","index":0}"#),
            [.blockStop(index: 0)], "text block stop")
        equal(
            acc.feed(
                #"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":15}}"#
            ),
            [.messageDelta(stopReason: "end_turn", outputTokens: 15)],
            "message_delta carries stop reason and cumulative tokens")
        equal(acc.feed(#"{"type":"message_stop"}"#), [.messageStop], "message_stop")
    }

    /// A tool input arriving in fragments, split mid-escape and mid-word: the
    /// concatenation is the contract, not any one fragment being parseable.
    static func fragmentedToolInput() {
        var acc = AnthropicStreamAccumulator()
        equal(
            acc.feed(toolStart(index: 0, id: "toolu_01A", name: "search_mail")),
            [.toolUseStarted(index: 0, id: "toolu_01A", name: "search_mail")],
            "tool block start announces id and name")

        // Full input: {"query": "dan's \"umbrella ☂\" invoice", "limit": 8}
        // The split lands between the backslash and the quote of an escape, and
        // in the middle of a multi-byte character's neighborhood.
        let fragments = [
            #"{"query": "dan's \"#,
            #""umbrella ☂"#,
            #"\" invoice", "li"#,
            #"mit": 8}"#,
        ]
        for fragment in fragments {
            equal(
                acc.feed(inputDelta(index: 0, fragment: fragment)), [],
                "input fragments accumulate silently")
        }
        equal(
            acc.feed(#"{"type":"content_block_stop","index":0}"#),
            [
                .toolUseComplete(
                    index: 0, id: "toolu_01A", name: "search_mail",
                    input: [
                        "query": .string("dan's \"umbrella ☂\" invoice"),
                        "limit": .number(8),
                    ]),
                .blockStop(index: 0),
            ],
            "fragments concatenate and parse on block stop")
    }

    /// No arguments is `{}` on this wire — an empty accumulation is a valid
    /// call, not a failure.
    static func noArgumentToolCall() {
        var acc = AnthropicStreamAccumulator()
        _ = acc.feed(toolStart(index: 0, id: "toolu_01B", name: "get_updates"))
        equal(
            acc.feed(#"{"type":"content_block_stop","index":0}"#),
            [
                .toolUseComplete(index: 0, id: "toolu_01B", name: "get_updates", input: [:]),
                .blockStop(index: 0),
            ],
            "no fragments parses as empty input, no error")
    }

    /// Unparseable input still round-trips: the block completes (with `[:]`),
    /// the error rides ALONGSIDE it, and the order is what Assistant.absorb
    /// depends on — complete first, so the error can mark `calls.last`.
    static func malformedToolInput() {
        var acc = AnthropicStreamAccumulator()
        _ = acc.feed(toolStart(index: 2, id: "toolu_01C", name: "send_email"))
        _ = acc.feed(inputDelta(index: 2, fragment: #"{"body": "unterminated"#))
        equal(
            acc.feed(#"{"type":"content_block_stop","index":2}"#),
            [
                .toolUseComplete(index: 2, id: "toolu_01C", name: "send_email", input: [:]),
                .providerError(
                    type: "malformed_tool_input",
                    message: "The assistant sent unreadable arguments for send_email."),
                .blockStop(index: 2),
            ],
            "malformed input: complete, then error, then stop — in that order")
    }

    /// The tolerance contract: garbage and the future are skipped, not thrown.
    static func garbageAndUnknownFrames() {
        var acc = AnthropicStreamAccumulator()
        equal(acc.feed("not json at all"), [], "non-JSON payload is skipped")
        equal(acc.feed(#"{"type":"ping"}"#), [], "ping is skipped")
        equal(
            acc.feed(#"{"type":"context_management_update","applied":true}"#), [],
            "an event type shipped after this was written is skipped")
        equal(
            acc.feed(#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"x"}}"#),
            [], "a delta with no index is skipped")
    }

    /// A fragment for a block that never opened has nowhere to accumulate.
    static func deltaForUnopenedBlock() {
        var acc = AnthropicStreamAccumulator()
        equal(
            acc.feed(inputDelta(index: 7, fragment: #"{"query": "x"}"#)), [],
            "input fragment for an unopened block is skipped")
        // Text deltas pass through undecorated by design — the session's turn
        // state owns text accumulation, so there is no block to consult.
        equal(
            acc.feed(
                #"{"type":"content_block_delta","index":7,"delta":{"type":"text_delta","text":"hi"}}"#
            ),
            [.textDelta(index: 7, text: "hi")],
            "text delta needs no open block")
    }

    /// Text and a tool call interleaved across two open blocks, stopped out of
    /// order: per-index state must not bleed between them.
    static func interleavedBlocks() {
        var acc = AnthropicStreamAccumulator()
        _ = acc.feed(
            #"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#)
        _ = acc.feed(toolStart(index: 1, id: "toolu_01D", name: "get_thread"))
        equal(
            acc.feed(
                #"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Checking"}}"#
            ),
            [.textDelta(index: 0, text: "Checking")], "text delta lands on its own block")
        _ = acc.feed(inputDelta(index: 1, fragment: #"{"thread_id": "t_9"}"#))
        equal(
            acc.feed(#"{"type":"content_block_stop","index":1}"#),
            [
                .toolUseComplete(
                    index: 1, id: "toolu_01D", name: "get_thread",
                    input: ["thread_id": .string("t_9")]),
                .blockStop(index: 1),
            ],
            "tool block closes with only its own fragments")
        equal(
            acc.feed(#"{"type":"content_block_stop","index":0}"#),
            [.blockStop(index: 0)], "text block closes as plain stop, out of order")
    }

    /// The provider's own error event.
    static func errorEvent() {
        var acc = AnthropicStreamAccumulator()
        equal(
            acc.feed(
                #"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#),
            [.providerError(type: "overloaded_error", message: "Overloaded")],
            "error event surfaces type and message")
        equal(
            acc.feed(#"{"type":"error","error":{}}"#),
            [
                .providerError(
                    type: nil, message: "The assistant provider reported an error.")
            ],
            "empty error still says something")
    }

    /// message_delta without usage: stop reason arrives, tokens stay unknown.
    static func messageDeltaWithoutUsage() {
        var acc = AnthropicStreamAccumulator()
        equal(
            acc.feed(#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#),
            [.messageDelta(stopReason: "tool_use", outputTokens: nil)],
            "usage is optional on message_delta")
    }

    /// The round-trip the echo depends on: parse → encode → parse lands on the
    /// same value, fields this code never named included.
    static func inputSurvivesReencoding() {
        let raw = #"{"query": "dan", "limit": 8, "flags": [true, null], "future_field": {"x": 1.5}}"#
        guard let first = try? JSONDecoder().decode([String: JSONValue].self, from: Data(raw.utf8)),
            let encoded = try? JSONEncoder().encode(first),
            let second = try? JSONDecoder().decode([String: JSONValue].self, from: encoded)
        else {
            fail("round-trip: decode/encode threw")
            return
        }
        equal(second, first, "JSONValue re-encodes to an equal value")
        equal(
            first["future_field"], .object(["x": .number(1.5)]),
            "unknown nested fields are carried, not dropped")
    }

    // MARK: - frame builders

    /// A content_block_start opening a tool_use block.
    static func toolStart(index: Int, id: String, name: String) -> String {
        #"{"type":"content_block_start","index":\#(index),"content_block":{"type":"tool_use","id":"\#(id)","name":"\#(name)","input":{}}}"#
    }

    /// An input_json_delta frame, with the fragment JSON-escaped the way the
    /// wire escapes it — by a JSON encoder, not by hand.
    static func inputDelta(index: Int, fragment: String) -> String {
        let escaped = String(
            decoding: try! JSONEncoder().encode([fragment]), as: UTF8.self)
            .dropFirst().dropLast()  // strip the [ ] around the lone element
        return
            #"{"type":"content_block_delta","index":\#(index),"delta":{"type":"input_json_delta","partial_json":\#(escaped)}}"#
    }

    // MARK: - assertions

    static func equal<T: Equatable>(
        _ got: T, _ want: T, _ label: String, line: Int = #line
    ) {
        checks += 1
        if got != want {
            failures += 1
            print("FAIL (line \(line)): \(label)\n  want: \(want)\n   got: \(got)")
        }
    }

    static func fail(_ label: String, line: Int = #line) {
        checks += 1
        failures += 1
        print("FAIL (line \(line)): \(label)")
    }
}
