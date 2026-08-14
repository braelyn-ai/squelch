// THE HOSTED assistant transport: streams a ⌘K ask through the user's own
// daemon (POST /client/assistant/messages), which forwards it to Passband's
// gateway under a DAEMON-HELD credential and pipes the SSE bytes back verbatim.
//
// Deliberately NOT in Keychain.swift. LLMProxy lives there because the BYOK
// key's keychain read is fileprivate and LLMProxy must be its only consumer —
// the file boundary IS the access rule. This proxy touches no key of any kind:
// it authenticates with the same human-door bearer token every other /client
// call carries, so it belongs out here with the rest of the assistant.

import Foundation

/// The relay's failures, in words worth showing. Kept apart from LLMError so
/// the copy can speak in plan terms ("your Passband plan") where BYOK errors
/// speak in key terms — the transcript surfaces `errorDescription` either way
/// (see `AssistantSession.errorText`).
enum RelayError: Error, LocalizedError {
    /// No stored connection settings — the connect gate is up.
    case notConnected
    case transport
    /// The daemon or the gateway said no: the HTTP status plus the friendliest
    /// copy the body offered (see `errorMessage`), nil when it offered none.
    case relay(status: Int, message: String?)

    var errorDescription: String? {
        switch self {
        case .notConnected: "Not connected to your Passband daemon."
        // Deliberately generic, matching LLMError.transport: a transport
        // failure must never quote anything from the request it carried.
        case .transport: "assistant request failed (network/tls)"
        case .relay(let status, let message):
            message ?? "assistant request failed (\(status))"
        }
    }
}

/// Makes relayed assistant calls. Same streaming contract as `LLMProxy.stream`:
/// the caller hands a fully-formed Anthropic request body and gets each SSE
/// frame's `data:` payload back — the daemon relays the gateway's bytes
/// verbatim, so the two transports are interchangeable at the call site.
enum RelayProxy {
    /// Its own session, same inactivity posture as LLMProxy's: on a
    /// `bytes(for:)` transfer the request timeout resets on every byte —
    /// generous between chunks, never a deadline on the whole generation.
    private static let streamSession = Sessions.ephemeral(timeout: streamTimeout)
    private static let streamTimeout: TimeInterval = 120

    /// REFUSES EVERY REDIRECT. The human-door bearer token rides in
    /// `Authorization`, and a 3xx from a compromised or hijacked host must
    /// never carry it to a host nobody chose. Same empty allow-list LLMProxy
    /// and EventStream keep on their token-bearing connections.
    private static let pinned = SchemePinned(allow: [])

    /// Cap on a non-200 body read to find the error text. Past this the
    /// upstream is broken or hostile and there is nothing worth quoting.
    private static let maxErrorBytes = 64 * 1024

    /// Make ONE streaming call through the daemon and yield each SSE frame's
    /// `data:` payload as it arrives. `body` is the same fully-formed Anthropic
    /// request body `LLMProxy.stream` takes, `"stream": true` already set.
    static func stream(body: Data) -> AsyncThrowingStream<String, Error> {
        AsyncThrowingStream { continuation in
            let task = Task {
                do {
                    // The ACTIVE account's daemon, read at call time: a relayed
                    // ask always goes where the conversation's mail lives.
                    guard let settings = await MainActor.run(body: { AppStore.shared.settings })
                    else { throw RelayError.notConnected }

                    // Trailing-slash strip, same as EventStream.buildRequest —
                    // the stored URL may or may not carry one.
                    var base = settings.serverURL
                    while base.hasSuffix("/") { base.removeLast() }
                    guard let url = URL(string: base + "/client/assistant/messages") else {
                        throw RelayError.notConnected
                    }

                    var req = URLRequest(url: url)
                    req.httpMethod = "POST"
                    req.setValue(
                        "Bearer \(settings.apiToken)", forHTTPHeaderField: "Authorization")
                    req.setValue("application/json", forHTTPHeaderField: "content-type")
                    req.setValue("text/event-stream", forHTTPHeaderField: "Accept")
                    req.timeoutInterval = streamTimeout
                    req.httpBody = body

                    let bytes: URLSession.AsyncBytes
                    let response: URLResponse
                    do {
                        (bytes, response) = try await streamSession.bytes(
                            for: req, delegate: Self.pinned)
                    } catch {
                        // Refused / DNS / TLS / inactivity. Never the error
                        // text: it can quote the request we just built, token
                        // included.
                        throw RelayError.transport
                    }
                    guard let http = response as? HTTPURLResponse else {
                        throw RelayError.transport
                    }
                    guard http.statusCode == 200 else {
                        throw RelayError.relay(
                            status: http.statusCode,
                            message: await errorMessage(status: http.statusCode, from: bytes))
                    }

                    var parser = SSEParser()
                    // Split by hand rather than with `bytes.lines`: AsyncLineSequence
                    // silently DROPS empty lines, and the blank line between frames
                    // is SSE's only frame terminator, so every event would sit
                    // unread in the buffer (see EventStream.connect). Splitting on
                    // LF is safe on UTF-8 — 0x0A cannot appear inside a multi-byte
                    // sequence.
                    var line: [UInt8] = []
                    do {
                        for try await byte in bytes {
                            guard byte == UInt8(ascii: "\n") else {
                                // A stream that never sends a newline would grow this
                                // buffer for as long as the answer runs. Past the cap
                                // the upstream is broken; fail rather than swell.
                                guard line.count < SSEParser.maxFrameBytes else {
                                    throw RelayError.transport
                                }
                                line.append(byte)
                                continue
                            }
                            try Task.checkCancellation()
                            if let frame = parser.feed(String(decoding: line, as: UTF8.self)) {
                                continuation.yield(frame.data)
                            }
                            line.removeAll(keepingCapacity: true)
                        }
                    } catch let error as RelayError {
                        throw error
                    } catch is CancellationError {
                        throw CancellationError()
                    } catch {
                        throw RelayError.transport
                    }
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            // A consumer that stops reading (or is cancelled) must take the
            // upstream connection down with it.
            continuation.onTermination = { _ in task.cancel() }
        }
    }

    /// Pull a bounded prefix of a non-200 body and turn it into copy worth
    /// showing. TWO shapes arrive on this wire: the daemon's own flat
    /// `{"error":"assistant_relay_unavailable"}` and the gateway's
    /// Anthropic-shaped `{"error":{"type":…,"message":…}}` passed through
    /// verbatim (same nested shape LLMProxy.errorMessage digs into). Never
    /// surfaces headers or the token.
    private static func errorMessage(
        status: Int, from bytes: URLSession.AsyncBytes
    ) async -> String? {
        var raw: [UInt8] = []
        do {
            for try await byte in bytes {
                raw.append(byte)
                if raw.count >= maxErrorBytes { break }
            }
        } catch {
            // A body that died mid-read is still worth parsing.
        }
        let object = (try? JSONSerialization.jsonObject(with: Data(raw))) as? [String: Any]

        // The daemon's own reasons (flat shape), in the user's terms.
        if let reason = object?["error"] as? String {
            switch reason {
            case "assistant_relay_unavailable":
                return "Your Passband plan's assistant isn't set up on this daemon."
            case "assistant_relay_unreachable":
                return "Your daemon couldn't reach Passband's assistant service. "
                    + "Try again in a moment."
            default:
                break  // an unknown flat reason falls through to the status copy
            }
        }

        // Statuses that deserve plan-language whatever the body said: the
        // gateway's own JSON for these talks about credits and rate limits in
        // ITS terms, which are not the user's.
        switch status {
        case 402:
            return "You've used this month's assistant budget on your Passband plan. "
                + "It resets at the start of next month."
        case 429:
            return "The assistant is busy right now. Try again in a moment."
        default:
            break
        }

        // A gateway error passed through verbatim: the nested Anthropic shape.
        if let error = object?["error"] as? [String: Any],
            let message = error["message"] as? String, !message.isEmpty
        {
            return message
        }
        return nil
    }
}
