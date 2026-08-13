// IS THIS A LOOKUP, OR IS IT A QUESTION? One text field on the phone answers
// both, and this decides which. "delta receipt" wants the search index;
// "when does my flight to denver leave" wants the agent, and typing it into a
// keyword search returns nothing useful and teaches the user that search is
// broken.
//
// HEURISTICS FIRST, MODEL LAST. Almost every string settles on shape alone —
// a trailing question mark, an interrogative opener, or simply being too short
// to be a sentence — and those verdicts cost nothing and land before the
// keystroke is over. Only the genuinely ambiguous middle ("what did stripe
// send about the payout") reaches a model, and then it is the smallest one
// there is, answering with a single word out of four tokens.
//
// EVERY FAILURE FALLS BACK TO SEARCH. No key, no network, a refusal, a
// cancelled call, a body that will not parse: the answer is `.search`. Search
// is the tab the user is already looking at, so being wrong that way is a
// non-event, while being wrong the other way yanks them into a different tab
// they never asked for. The network path is skipped ENTIRELY when no Anthropic
// key is configured — an unconfigured user must not pay a keychain miss and a
// round trip per pause in their typing.

import Foundation

/// Which surface a typed string belongs to.
enum QueryVerdict: Sendable, Equatable {
    case search
    case question
}

/// The decision, with its own isolation and its own memory. An actor rather
/// than a plain helper because it holds two pieces of mutable state that a
/// fast typist would otherwise race: the single in-flight classification, and
/// the verdicts already paid for.
actor QueryClassifier {
    /// Openers that make a sentence a question even without the mark. Only the
    /// ones that CANNOT open a keyword search: "mail", "from" and friends are
    /// deliberately absent, because "from stripe" is a lookup.
    private static let interrogatives: Set<String> = [
        "who", "what", "when", "where", "why", "how",
        "can", "could", "should", "would",
        "does", "do", "is", "are", "will", "did",
    ]

    /// Below this a string is a search term no matter what it says. Two words
    /// is a name or a vendor; fifteen characters is roughly where a sentence
    /// starts being possible at all.
    private static let minQuestionWords = 3
    private static let minQuestionLength = 15

    /// Verdicts already decided, so re-typing (or backspacing back into) a
    /// string that has been classified never spends a second call. Bounded
    /// because it is fed by keystrokes: an unbounded table here grows for as
    /// long as the app is open. Only MODEL answers land here — a heuristic
    /// verdict is cheaper to recompute than to look up.
    private var memo = LRUMap<String, QueryVerdict>(limit: 128)

    /// The one classification allowed to be in flight. A newer keystroke
    /// cancels it: the answer to a string the user has already moved past is
    /// worthless, and paying for it would also let two answers land out of
    /// order and hand the wrong term to the agent.
    private var inFlight: Task<QueryVerdict?, Never>?

    /// Shape alone, no network. nil = genuinely ambiguous, ask a model.
    static func heuristic(_ text: String) -> QueryVerdict? {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return .search }
        // The mark settles it outright, and BEFORE the length floors: "why?" is
        // three characters and unambiguously a question.
        if trimmed.hasSuffix("?") { return .question }

        let words = trimmed.split(whereSeparator: \.isWhitespace)
        if let first = words.first,
            interrogatives.contains(first.lowercased()),
            words.count >= minQuestionWords
        {
            return .question
        }
        // Too short to be a sentence. Note this runs AFTER the opener check, so
        // "is this spam" (three words) is still a question.
        if words.count < minQuestionWords || trimmed.count < minQuestionLength {
            return .search
        }
        return nil
    }

    /// The verdict for one string. Returns as soon as the heuristics can speak;
    /// otherwise awaits at most one model call, which a newer caller cancels
    /// out from under this one (a cancelled classification resolves to
    /// `.search`, so the stale caller quietly does nothing).
    func verdict(for raw: String) async -> QueryVerdict {
        let text = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if let quick = Self.heuristic(text) { return quick }
        if let remembered = memo.get(text) { return remembered }

        inFlight?.cancel()
        let task = Task { await Self.ask(text) }
        inFlight = task
        let answer = await task.value
        // nil is "no usable answer" — cancelled, no key, or the call failed.
        // NOT memoized: caching a cancellation as `.search` would make the
        // string permanently wrong for the rest of the session.
        guard let answer else { return .search }
        memo.set(text, answer)
        return answer
    }

    /// The model call. Static and nonisolated so it runs off the actor while
    /// the actor stays free to take the next keystroke.
    private static func ask(_ text: String) async -> QueryVerdict? {
        // The one gate that keeps an unconfigured install entirely off the
        // network. `statusAsync` also answers WHICH provider, and this prompt
        // names a Claude model: an OpenAI key would be spent on a body its
        // endpoint cannot read.
        let key = await AssistantKeyStore.statusAsync()
        guard key.present, key.provider == .anthropic else { return nil }
        guard !Task.isCancelled else { return nil }

        // Four tokens is enough for one word, and the cap is what keeps a
        // chatty model from turning a keystroke into a paragraph the user pays
        // for. The system prompt carries the whole instruction so the user turn
        // can be the raw text with nothing wrapped around it — anything we
        // added there would be text the model might classify instead.
        let body: [String: Any] = [
            "model": "claude-haiku-4-5",
            "max_tokens": 4,
            "system": """
                You classify text a user typed into an email app's search field. \
                Reply with exactly one word. SEARCH if it reads as a keyword \
                lookup for finding emails. QUESTION if it reads as a natural \
                language question or request for an assistant.
                """,
            "messages": [["role": "user", "content": text]],
        ]

        guard let answer = try? await LLMProxy.complete(body: body) else { return nil }
        guard !Task.isCancelled else { return nil }
        // Anything that is not an affirmative QUESTION is a search, including a
        // model that decided to explain itself instead of answering.
        return answer.uppercased().contains("QUESTION") ? .question : .search
    }
}
