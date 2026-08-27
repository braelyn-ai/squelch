// The BYOK assistant key and the proxy that is allowed to see it. Split from
// Keychain.swift so the human-door credential store can compile into the
// notification extension without the LLM proxy going with it — an extension
// that only ever reads a bearer token has no business linking a code path that
// holds the user's provider key.
//
// The rule the split preserves: `AssistantKeyStore.read()` is fileprivate and
// `LLMProxy` lives in THIS file because it is that function's only consumer.
// Every LLMProxy entry point holds the line — `complete()` and `stream()` each
// read the key inside themselves and hand back only provider output, so the key
// is never a parameter, a returned value, or anything an error carries. A
// caller wanting a one-shot answer gets its entry point here rather than
// building its own request elsewhere: the moment a second file constructs the
// header, the key has to leave this one.

import Foundation
import Security

/// BYOK assistant key slot — entirely separate from the human-door tokens in
/// [`SettingsStore`], and GLOBAL rather than per-account: the key is the
/// human's, not the mailbox's, and one assistant serves every account.
private let accountAssistantKey = "assistant_api_key"

// MARK: - BYOK assistant key

enum AssistantProvider: String, Sendable {
    case anthropic, openai

    var label: String {
        switch self {
        case .anthropic: "Anthropic"
        case .openai: "OpenAI"
        }
    }
}

/// Whether an assistant key is stored, and (if so) which provider it routes to.
/// Lets Settings show "key set — Anthropic" without ever handling the secret.
struct AssistantKeyStatus: Sendable, Equatable {
    var present: Bool
    var provider: AssistantProvider?

    static let absent = AssistantKeyStatus(present: false, provider: nil)
}

enum AssistantKeyStore {
    /// Provider inferred from the key prefix, matching the server-side Stage-2
    /// routing. Never exposes the key value.
    ///
    /// `nil` for anything that matches NEITHER prefix, and that is a safety
    /// property, not pedantry: an iOS keyboard that auto-capitalized a
    /// hand-typed key ("Sk-ant-…") used to fall through to the OpenAI arm and
    /// POST an Anthropic key to api.openai.com. An unrecognized key now routes
    /// nowhere.
    fileprivate static func provider(forKey key: String) -> AssistantProvider? {
        if key.hasPrefix("sk-ant-") { return .anthropic }
        if key.hasPrefix("sk-") { return .openai }
        return nil
    }

    /// No provider's key ever contains whitespace, but a pasted one can — a
    /// terminal that soft-wrapped the key embeds a newline mid-string, edge
    /// trimming misses it, and URLSession then silently DROPS the whole
    /// `x-api-key` header rather than send an invalid value. The provider's
    /// answer ("x-api-key header is required") says nothing about why. So
    /// whitespace is stripped wholesale, on the way in AND out — out too, so a
    /// key stored before this rule heals at first use instead of demanding a
    /// re-paste.
    private static func sanitized(_ key: String) -> String {
        key.filter { !$0.isWhitespace }
    }

    /// The one function allowed to see the secret. `fileprivate` on purpose:
    /// nothing outside this file — the view layer especially — can obtain the
    /// key.
    fileprivate static func read() -> String? {
        guard let k = try? Keychain.read(account: accountAssistantKey) else { return nil }
        let key = sanitized(k)
        return key.isEmpty ? nil : key
    }

    static func status() -> AssistantKeyStatus {
        guard let k = read() else { return .absent }
        return AssistantKeyStatus(present: true, provider: provider(forKey: k))
    }

    /// Off-main-actor status read — same prompt-blocking reason as
    /// `SettingsStore.loadAsync`. Still never yields the key itself.
    static func statusAsync() async -> AssistantKeyStatus {
        await offMain { status() }
    }

    /// Hand the stored key to Settings' Show / Edit affordance — the one
    /// deliberate hole in the rule above, and nothing else may call it. Off the
    /// main actor for the same prompt-blocking reason as `statusAsync`.
    static func revealAsync() async -> String? {
        await offMain { read() }
    }

    /// Store the user's assistant key. Never logged, never echoed.
    static func set(_ key: String) throws {
        try Keychain.write(account: accountAssistantKey, value: sanitized(key))
    }

    /// Forget the stored assistant key.
    static func clear() throws { try Keychain.delete(account: accountAssistantKey) }
}

// MARK: - BYOK LLM proxy

/// Result of one LLM round-trip: the upstream HTTP status plus the raw JSON
/// body (an Anthropic message, an OpenAI completion, or a provider error — the
/// caller inspects `status` and shapes accordingly).
struct LLMResponse: Sendable {
    var status: Int
    var json: Data
}

enum LLMError: Error, LocalizedError {
    case noKey
    case wrongProvider
    case transport
    case nonJSON
    /// Upstream said no: the HTTP status plus whatever `error.message` the
    /// provider's JSON body carried (nil when the body wasn't parseable).
    case provider(status: Int, message: String?)

    var errorDescription: String? {
        switch self {
        case .noKey: "No assistant key set — add one in Settings."
        case .wrongProvider:
            "Streaming needs an Anthropic key (sk-ant-…). Paste one in Settings."
        // Deliberately generic: an upstream/transport failure must never
        // surface anything that could include the key.
        case .transport: "assistant request failed (network/tls)"
        case .nonJSON: "assistant returned a non-JSON body"
        // The provider's own words when we have them, matching what
        // `complete()`'s callers already show from `parsed.error?.message`.
        case .provider(let status, let message):
            message ?? "assistant request failed (\(status))"
        }
    }
}

/// Makes assistant completion calls. Routes by the key's real prefix, never a
/// caller-supplied provider. The key never leaves this type — not a parameter,
/// not a return value, not in any error.
enum LLMProxy {
    private static let session = Sessions.ephemeral(timeout: 120)

    /// Its own session for streamed calls. On a `bytes(for:)` transfer the
    /// request timeout is an INACTIVITY timeout — URLSession resets it on every
    /// byte — which is exactly what a long answer needs: generous between
    /// chunks, never a deadline on the whole generation.
    private static let streamSession = Sessions.ephemeral(timeout: streamTimeout)
    private static let streamTimeout: TimeInterval = 120

    /// REFUSES EVERY REDIRECT, on both sessions. URLSession strips
    /// `Authorization` across origins but carries custom headers — `x-api-key`
    /// above all — verbatim through a hop, so a 302 from a compromised or
    /// hijacked provider host would post the user's own BYOK key to whatever
    /// host it named, and the user would see only "assistant request failed".
    /// Neither provider legitimately 3xx's /v1/messages, so refusing costs
    /// nothing: the 3xx surfaces to the caller as a non-200. Same empty
    /// allow-list EventStream keeps on the token-bearing feed.
    private static let pinned = SchemePinned(allow: [])

    /// Cap on a non-200 body we read to find the provider's error text. Past
    /// this the upstream is broken or hostile and there is nothing to quote.
    private static let maxErrorBytes = 64 * 1024

    /// Make ONE completion call. `body` is a fully-formed provider request body
    /// MINUS auth (model, messages, tools, max_tokens, …).
    ///
    /// `require` enforces a provider IN-BAND, at the same keychain read that
    /// decides the routing: a caller whose body names a Claude model passes
    /// `.anthropic` and can never race a key swap into posting that body to a
    /// host that would not understand it. A status() precheck in the caller is
    /// a courtesy that saves a round trip; this is the guarantee.
    static func complete(body: Data, require: AssistantProvider? = nil) async throws -> LLMResponse
    {
        guard let key = AssistantKeyStore.read() else { throw LLMError.noKey }
        let provider = AssistantKeyStore.provider(forKey: key)
        if let require, provider != require { throw LLMError.wrongProvider }

        var req: URLRequest
        switch provider {
        case .anthropic:
            req = URLRequest(url: URL(string: "https://api.anthropic.com/v1/messages")!)
            req.setValue(key, forHTTPHeaderField: "x-api-key")
            req.setValue("2023-06-01", forHTTPHeaderField: "anthropic-version")
        case .openai:
            req = URLRequest(url: URL(string: "https://api.openai.com/v1/chat/completions")!)
            req.setValue("Bearer \(key)", forHTTPHeaderField: "authorization")
        case nil:
            // A key that matches no known prefix routes NOWHERE. See
            // provider(forKey:) for why this case exists.
            throw LLMError.wrongProvider
        }
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "content-type")
        req.httpBody = body

        let data: Data
        let response: URLResponse
        do {
            (data, response) = try await session.data(for: req, delegate: Self.pinned)
        } catch {
            throw LLMError.transport
        }
        guard let http = response as? HTTPURLResponse else { throw LLMError.transport }
        // Validate it parses as JSON so callers can assume a decodable body.
        guard (try? JSONSerialization.jsonObject(with: data)) != nil else { throw LLMError.nonJSON }
        return LLMResponse(status: http.statusCode, json: data)
    }

    /// Make ONE streaming call and yield each SSE frame's `data:` payload as it
    /// arrives. `body` is a fully-formed Anthropic request body MINUS auth, with
    /// `"stream": true` already set by the caller.
    ///
    /// Same invariant as `complete()`: the key is read HERE, inside the task,
    /// and never leaves — not a parameter, not a yielded element, not inside any
    /// error this can throw. Anthropic only: an OpenAI key finishes the stream
    /// with `.wrongProvider` rather than posting a body no one can read to a
    /// host that never agreed to see it.
    static func stream(body: Data) -> AsyncThrowingStream<String, Error> {
        AsyncThrowingStream { continuation in
            let task = Task {
                do {
                    guard let key = AssistantKeyStore.read() else { throw LLMError.noKey }
                    guard AssistantKeyStore.provider(forKey: key) == .anthropic else {
                        throw LLMError.wrongProvider
                    }

                    var req = URLRequest(url: URL(string: "https://api.anthropic.com/v1/messages")!)
                    req.httpMethod = "POST"
                    req.setValue(key, forHTTPHeaderField: "x-api-key")
                    req.setValue("2023-06-01", forHTTPHeaderField: "anthropic-version")
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
                        // Refused / DNS / TLS / inactivity. Never the error text:
                        // it can quote the request we just built.
                        throw LLMError.transport
                    }
                    guard let http = response as? HTTPURLResponse else { throw LLMError.transport }
                    guard http.statusCode == 200 else {
                        throw LLMError.provider(
                            status: http.statusCode, message: await errorMessage(from: bytes))
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
                                    throw LLMError.transport
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
                    } catch let error as LLMError {
                        throw error
                    } catch is CancellationError {
                        throw CancellationError()
                    } catch {
                        throw LLMError.transport
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

    /// Pull a bounded prefix of a non-200 body and dig out `error.message`.
    /// Returns nil for anything unparseable; never surfaces headers or the key.
    private static func errorMessage(from bytes: URLSession.AsyncBytes) async -> String? {
        var raw: [UInt8] = []
        do {
            for try await byte in bytes {
                raw.append(byte)
                if raw.count >= maxErrorBytes { break }
            }
        } catch {
            // A body that died mid-read is still worth parsing.
        }
        return errorMessage(inJSON: Data(raw))
    }

    /// The dig itself, shared by the streamed and the buffered paths so both
    /// quote the provider the same way. Returns nil for anything unparseable;
    /// never surfaces headers or the key.
    private static func errorMessage(inJSON data: Data) -> String? {
        let json = try? JSONSerialization.jsonObject(with: data)
        guard let object = json as? [String: Any],
            let error = object["error"] as? [String: Any],
            let message = error["message"] as? String, !message.isEmpty
        else { return nil }
        return message
    }
}
