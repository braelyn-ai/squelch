// KEYWORD OR QUESTION: the rule that decides whether a settled search is a
// lookup the results list already answered, or a question worth spending a
// model on (docs/SEARCH.md §5).
//
// A RULE, NOT A MODEL, and that is the whole point. The phone counts spaces
// (`MobileSearchView`), and its comment states the principle this inherits: a
// rule that is wrong in a way anybody can see, and fix by deleting a word,
// beats a model deciding which door you meant for a round trip, on your key,
// wrongly in ways nobody can predict. The Mac gets a better rule, not a model.
//
// Two signals, both free. The SHAPE of the query is the reader's own doing:
// question words, a question mark, "my", or simply enough words to be a
// sentence. What RETRIEVAL DID with it is the daemon's (`SearchDiagnostics`):
// three or more words that co-occur in no single message is exactly the shape
// of "abstract conference wifi password", where the words are right and the
// mail just does not use all of them. That second signal is ABSENT against an
// older daemon, which is why every rule here still decides without it.
//
// Foundation only, and free of app types beyond the wire struct it reads, so
// test.sh compiles it alone: a ranking-adjacent decision that fires on somebody
// else's mailbox is asserted fixture by fixture rather than reasoned about.

import Foundation

enum SearchIntent {

    /// What the panel should do with a settled query.
    enum Verdict: Equatable, Sendable {
        /// The results list is the answer. Nothing else runs.
        case lookup
        /// Worth reading candidates and reasoning over them: start (or refine)
        /// the agent lane, and say which signal asked for it.
        case deeper(trigger: Trigger)

        var isDeeper: Bool {
            if case .deeper = self { return true }
            return false
        }

        var trigger: Trigger? {
            if case .deeper(let trigger) = self { return trigger }
            return nil
        }
    }

    /// WHY the lane ran, in the two flavours the band can explain.
    enum Trigger: Equatable, Sendable {
        /// The reader wrote a question, or something long enough to be one.
        case questionShaped
        /// Three or more words and no message carries all of them.
        case noStrictHits

        /// The closed-vocabulary string `search_deeper_started` carries. It is
        /// in `Analytics.allowedStrings`; nothing derived from the query or the
        /// mail is anywhere near this event.
        var analyticsValue: String {
            switch self {
            case .questionShaped: "question"
            case .noStrictHits: "no_strict_hits"
            }
        }
    }

    /// Words that open a question. `should`/`could` are here because "should I
    /// have replied to the landlord" is a question about mail even though it
    /// never reaches a question mark.
    private static let questionWords: Set<String> = [
        "what", "when", "where", "who", "which", "how",
        "did", "do", "does", "is", "was", "can", "could", "should",
    ]

    /// Somebody talking about their own mailbox rather than naming a thing in
    /// it. "my wifi password" is a question; "wifi password" is a lookup.
    private static let firstPerson: Set<String> = ["my", "i", "me", "mine"]

    /// The operator prefixes `parse_search_query` owns. A query carrying one is
    /// STRUCTURED INTENT: the reader is already speaking the index's language,
    /// so the lane never second-guesses them, whatever the rest of the words
    /// look like.
    private static let operatorPrefixes = ["from:", "after:", "before:"]

    /// Enough words to be a sentence rather than a name. Four is a phrase
    /// somebody might file mail under; five is somebody talking.
    private static let sentenceLength = 5

    /// The fewest plain words that can carry the "no message has all of these"
    /// signal. Under three, a strict miss is usually a typo or a proper noun
    /// this mailbox has never seen, and there is nothing for a model to read.
    private static let coOccurrenceFloor = 3

    /// Judge ONE settled query, after the fetch that answered it.
    ///
    /// `diagnostics` is what the daemon said about THIS query — nil from a
    /// daemon that predates wave 1, and nil is a real answer here rather than a
    /// missing one: without counts the shape of the query is the only signal,
    /// and a four-word phrase that might have had zero strict hits stays a
    /// lookup rather than being guessed into a model call.
    static func classify(query: String, diagnostics: SearchDiagnostics?) -> Verdict {
        let words = terms(query)
        guard !words.isEmpty else { return .lookup }
        // AND NOT-EMPTY IS NOT THE SAME AS HAVING WORDS IN IT. A lone "?" left
        // behind by a deleted query is one token, is not an operator, and ends
        // in a question mark: without this it reads as question-shaped and,
        // under `automatic`, starts a paid conversation whose entire user turn
        // is one punctuation mark. §5's "one or two plain words are never
        // deeper" applies harder to none, so a query with no letters anywhere
        // in it is a lookup whatever the counts say about it.
        guard words.contains(where: { $0.contains(where: \.isLetter) }) else { return .lookup }
        // OPERATORS WIN, ALWAYS, before anything else is looked at.
        if words.contains(where: isOperator) { return .lookup }
        if isQuestionShaped(words: words, query: query) {
            return .deeper(trigger: .questionShaped)
        }
        // The daemon's own account of the search. Three or more words that no
        // one message carries together is the motivating query's shape.
        if words.count >= coOccurrenceFloor, let diagnostics, diagnostics.strict_hits == 0 {
            return .deeper(trigger: .noStrictHits)
        }
        return .lookup
    }

    /// One line for the band, so a reader who did not want the lane can see why
    /// it ran and shorten the query. Takes the query back because the count in
    /// it has to be the reader's own words, not a number the trigger carries
    /// around after the fact.
    static func reason(_ trigger: Trigger, query: String) -> String {
        switch trigger {
        case .questionShaped:
            return "that reads like a question"
        case .noStrictHits:
            let count = terms(query).count
            return "no email has all \(count) of those words"
        }
    }

    // MARK: - the shape of a query

    /// Whitespace-separated words, exactly as §5 counts them.
    static func terms(_ query: String) -> [String] {
        query.split(whereSeparator: \.isWhitespace).map(String.init)
    }

    private static func isOperator(_ token: String) -> Bool {
        let lowered = token.lowercased()
        return operatorPrefixes.contains { lowered.hasPrefix($0) }
    }

    private static func isQuestionShaped(words: [String], query: String) -> Bool {
        if words.count >= sentenceLength { return true }
        if query.trimmingCharacters(in: .whitespacesAndNewlines).hasSuffix("?") { return true }
        if let first = words.first, questionWords.contains(bare(first)) { return true }
        // WHOLE WORDS. "i" as a substring is in half the mail ever written, and
        // "my" is inside "myself"; a marker matched loosely would send every
        // second lookup to a model.
        return words.contains { firstPerson.contains(bare($0)) }
    }

    /// The word inside a token somebody typed punctuation around: case-folded,
    /// leading punctuation dropped, and cut at the first non-letter after it.
    /// "What?" is "what" and "where's" is "where", while "mycompany" stays
    /// itself — the cut is at the END of the word, so a marker can never match
    /// a longer word that merely begins with it.
    ///
    /// A TOKEN WITH A DIGIT IN IT IS NOT A WORD, and that is not a nicety: the
    /// cut above stops at the first non-letter, so "i9" would reduce to "i",
    /// "my2024" to "my" and "can8" to "can", and "form i9" — two words, an
    /// entirely ordinary mailbox lookup — would read as somebody talking about
    /// their own mail and start a model on their key. Nothing in either marker
    /// set has a digit in it, so refusing the whole token costs nothing.
    private static func bare(_ token: String) -> String {
        guard !token.contains(where: \.isNumber) else { return "" }
        return String(token.lowercased().drop { !$0.isLetter }.prefix { $0.isLetter })
    }
}
