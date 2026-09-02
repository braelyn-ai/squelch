// The `from:` operator as the search field sees it while it is being typed.
//
// The daemon parses `from:<text>` out of a finished query (search_query.rs);
// this is the OTHER end of that operator, the one that decides when the search
// field should be offering senders and what accepting one does to the text.
//
// THE TRAILING TOKEN IS THE CARET. A SwiftUI TextField does not tell us where
// the caret is, and RecipientField gets by without it by parsing the whole
// string into pills and a fragment. The search field's equivalent: the reader
// is "in" a `from:` operator exactly while the LAST whitespace-separated token
// starts with `from:` and the text does not end in whitespace. A space after
// the address closes the operator (the way it closes a recipient pill), and a
// `from:` earlier in the query is finished business the reader has moved past.
// Wrong in a way anybody can see: if the menu is up and you did not want it,
// type a space.
//
// Pure Foundation, no SwiftUI, so test.sh can hold it up alone.

import Foundation

enum FromOperator {
    /// The operator's spelling, matched case-insensitively so `From:` and
    /// `FROM:` open the menu too. The daemon's parser is the same way.
    private static let prefix = "from:"

    /// The text after `from:` in the trailing token, "" when the reader has
    /// typed the operator and nothing after it yet (the menu then lists the
    /// senders with the most mail), and nil when the reader is not in a
    /// `from:` operator at all.
    static func fragment(in query: String) -> String? {
        guard let token = trailingToken(of: query) else { return nil }
        guard token.count >= prefix.count,
            token.prefix(prefix.count).lowercased() == prefix
        else { return nil }
        return String(token.dropFirst(prefix.count))
    }

    /// The query with the trailing `from:` token replaced by `from:<address>`
    /// and a closing space, so the operator is complete and the next keystroke
    /// starts a new word. A query that is not in a `from:` operator comes back
    /// unchanged: accepting a suggestion that is no longer on offer must not
    /// rewrite the reader's text.
    static func accepting(_ address: String, in query: String) -> String {
        guard fragment(in: query) != nil, let token = trailingToken(of: query) else {
            return query
        }
        let head = query.dropLast(token.count)
        return "\(head)\(prefix)\(address) "
    }

    /// The last whitespace-separated token, or nil when the text is empty or
    /// ends in whitespace (the caret is past the last token, so no token is
    /// being typed).
    private static func trailingToken(of query: String) -> Substring? {
        guard let last = query.unicodeScalars.last,
            !CharacterSet.whitespacesAndNewlines.contains(last)
        else { return nil }
        if let cut = query.lastIndex(where: { $0.isWhitespace || $0.isNewline }) {
            return query[query.index(after: cut)...]
        }
        return query[...]
    }
}
