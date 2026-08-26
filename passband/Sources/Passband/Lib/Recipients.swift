// WHO A MESSAGE IS ADDRESSED TO, as one value instead of three loose strings.
//
// Every composer in the app — the pane, the reader's inline reply, the phone's
// sheet — holds the same three comma-joined header values, and every one of
// them needs the same four operations: split a list into the tokens a human
// clicked on, move one of those tokens to another field, drop it, and count
// what is left. Doing that against `String` at each call site is how To and Cc
// end up disagreeing about who is on a message.
//
// PURE ON PURPOSE: no SwiftUI, no store, no wire. The composer state owns three
// strings (they are what goes on the wire, verbatim); this type is a VIEW of
// them that knows the rules. Which is also what lets the suite test the one
// operation with real consequences — moving somebody to Bcc — without an app.
//
// THE MOVE IS THE POINT. `move(_:to:)` takes an address out of every field
// before putting it in the destination, so an address is in exactly one of the
// three when it returns. Anything less is how a person ends up in To and Bcc at
// once, which is not a blind copy of anything: their address is right there in
// the header everyone can read, and the sender believed otherwise.

import Foundation

/// Which of the three recipient headers. Ordered as they are written and read.
enum RecipientSlot: String, CaseIterable, Sendable, Hashable {
    case to, cc, bcc

    /// The header's own lowercase spelling — what the composer labels the field
    /// with, and what review calls the row.
    var label: String { rawValue }
}

/// The three recipient headers of one composition, each a comma-joined list
/// exactly as it goes out.
struct Recipients: Sendable, Equatable {
    var to: String = ""
    var cc: String = ""
    var bcc: String = ""

    init(to: String = "", cc: String = "", bcc: String = "") {
        self.to = to
        self.cc = cc
        self.bcc = bcc
    }

    subscript(slot: RecipientSlot) -> String {
        get {
            switch slot {
            case .to: to
            case .cc: cc
            case .bcc: bcc
            }
        }
        set {
            switch slot {
            case .to: to = newValue
            case .cc: cc = newValue
            case .bcc: bcc = newValue
            }
        }
    }

    /// The tokens in one field, as the sender wrote them — display names and
    /// all. What the composer draws pills from.
    func tokens(_ slot: RecipientSlot) -> [String] { Self.split(self[slot]) }

    /// How many people one field names.
    func count(_ slot: RecipientSlot) -> Int { tokens(slot).count }

    /// Every addressee, in header order. Used for the "reaches N people" lines.
    var all: [String] { RecipientSlot.allCases.flatMap(tokens) }

    /// Which field holds this address, if any.
    func slot(of token: String) -> RecipientSlot? {
        let wanted = Self.key(token)
        guard !wanted.isEmpty else { return nil }
        return RecipientSlot.allCases.first { slot in
            tokens(slot).contains { Self.key($0) == wanted }
        }
    }

    /// MOVE ONE ADDRESSEE TO ANOTHER FIELD, carrying the token whole — display
    /// name included, because that is how the sender addressed them.
    ///
    /// Removed from EVERY field before it is appended, not just from the one it
    /// came from. Two reasons, and the second is the one with teeth: the caller
    /// does not have to know where the token was, and a person who somehow ended
    /// up in two lists is repaired rather than duplicated. Leaving them in To
    /// while adding them to Bcc would be the worst outcome available — their
    /// address sits in a header every recipient reads, and the sender believes
    /// they blind-copied someone.
    ///
    /// Matching is on the BARE MAILBOX, case-folded, so `Bob <bob@x>` and
    /// `BOB@X` are one person however the two lists happen to spell them.
    mutating func move(_ token: String, to destination: RecipientSlot) {
        let key = Self.key(token)
        guard !key.isEmpty else { return }
        let carried = token.trimmingCharacters(in: .whitespacesAndNewlines)
        remove(token)
        var addrs = tokens(destination)
        addrs.append(carried)
        self[destination] = Self.join(addrs)
    }

    /// Drop an addressee from wherever they are. Idempotent: removing someone
    /// who was never there is not an error, it is the state you asked for.
    mutating func remove(_ token: String) {
        let key = Self.key(token)
        guard !key.isEmpty else { return }
        for slot in RecipientSlot.allCases {
            self[slot] = Self.join(tokens(slot).filter { Self.key($0) != key })
        }
    }

    /// Add an addressee to one field, unless they are ALREADY somewhere. The
    /// existing placement wins — an autocomplete hit must not silently promote
    /// someone out of Bcc back into the header everyone can read.
    mutating func add(_ token: String, to destination: RecipientSlot) {
        let key = Self.key(token)
        guard !key.isEmpty, slot(of: token) == nil else { return }
        var addrs = tokens(destination)
        addrs.append(token.trimmingCharacters(in: .whitespacesAndNewlines))
        self[destination] = Self.join(addrs)
    }

    // MARK: - list grammar

    /// Split an address-list header on the commas that SEPARATE addresses. A
    /// comma inside a quoted display name (`"Doe, John" <j@x>`) is part of the
    /// name, and splitting there invents a recipient who does not exist — and,
    /// worse, destroys the one who did.
    static func split(_ list: String) -> [String] {
        var out: [String] = []
        var current = ""
        var quoted = false
        for ch in list {
            if ch == "\"" { quoted.toggle() }
            if ch == ",", !quoted {
                out.append(current)
                current = ""
            } else {
                current.append(ch)
            }
        }
        out.append(current)
        return out
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty }
    }

    static func join(_ addrs: [String]) -> String {
        addrs.joined(separator: ", ")
    }

    /// THE IDENTITY OF AN ADDRESSEE: the bare mailbox, case-folded. Everything
    /// that compares two recipients compares this, so the same person written
    /// three ways is still one person.
    ///
    /// `Bob Smith <bob@x.test>` and `"Doe, John" <j@x.test>` yield the address
    /// between the angle brackets; a bare `bob@x.test` yields itself. Anything
    /// with no `@` at all yields "", which every caller treats as "not an
    /// address yet" — that is the half-typed fragment in a live field, and it
    /// must never match another one.
    static func key(_ token: String) -> String {
        let trimmed = token.trimmingCharacters(in: .whitespacesAndNewlines)
        var addr = trimmed
        // The LAST angle pair: a display name may legitimately contain one
        // (`"<the boss>" <b@x>`), and the address is always the final bracket.
        if let open = trimmed.lastIndex(of: "<"), let close = trimmed.lastIndex(of: ">"),
            open < close
        {
            addr = String(trimmed[trimmed.index(after: open)..<close])
        }
        addr = addr.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        return addr.contains("@") ? addr : ""
    }
}
