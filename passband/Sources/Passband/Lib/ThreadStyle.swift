// HOW A THREAD IS DRAWN. `classic` is the stack of email cards the reader has
// always been; `bubbles` reads the same mail as a conversation, with the user's
// own side right-aligned.
//
// NOTHING IS DETECTED. A thread is drawn the way the global default says until
// somebody says otherwise for that one thread, and that answer is remembered
// (ThreadStyleLedger). A back-and-forth that looks like chat but is really a
// receipt trail is a judgement no heuristic gets to make on the reader's behalf.
//
// PURE ON PURPOSE — no SwiftUI, no account, nothing to construct. The minimap's
// arithmetic takes one of these (a bubble is a narrower measure, so it is a
// different number of lines), and that file is asserted by a test suite built
// from source files alone.

import Foundation

enum ThreadStyle: String, CaseIterable, Sendable {
    case classic, bubbles

    var label: String {
        switch self {
        case .classic: "Email"
        case .bubbles: "Chat"
        }
    }

    /// The other one, which is what the reader's toggle switches to.
    var flipped: ThreadStyle {
        self == .classic ? .bubbles : .classic
    }

    /// Named for where the button GOES, not for where it is: the control shows
    /// the style you are one press away from.
    var symbol: String {
        switch self {
        case .classic: "list.bullet.rectangle"
        case .bubbles: "bubble.left.and.bubble.right"
        }
    }

    var actionLabel: String {
        switch self {
        case .classic: "email style"
        case .bubbles: "chat style"
        }
    }

    var actionHelp: String {
        switch self {
        case .classic: "read this thread as email cards"
        case .bubbles: "read this thread as chat bubbles"
        }
    }

    /// The per-message key the frame pool and the height memory file a rendered
    /// document under. THE STYLE IS PART OF IT because both are width-dependent:
    /// a bubble measures its document at the bubble's measure, and handing that
    /// frame (or that remembered height) to a full-width card paints the message
    /// at the wrong size until it re-measures. Classic keeps the bare message id
    /// so nothing that already clears by id has to learn a second spelling.
    func frameKey(_ messageId: Int) -> String {
        self == .classic ? String(messageId) : "\(messageId).chat"
    }
}
