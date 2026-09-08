// THE SETTING THAT DECIDES WHETHER A SEARCH MAY SPEND MONEY, and the rule that
// turns it, plus a verdict, into one move on the lane (docs/SEARCH.md §6.5).
//
// The preference and the rule live together, away from Prefs (which is storage
// and UserDefaults) and away from the panel (which is SwiftUI), because the
// rule is the second place in this feature where being wrong costs the reader
// money: the classifier decides whether a query is a question, and this decides
// whether that verdict is allowed to start anything. Both are asserted fixture
// by fixture in test.sh rather than reasoned about inside a live panel.
//
// The move is a VALUE the caller executes. That is what lets the two entry
// points — a settled query, and the picker being flipped under a lane that is
// already running — share the one rule about `off` instead of each carrying
// their own copy of it, which is how "off" came to mean "not started again"
// rather than "not running".

import Foundation

/// WHETHER A SEARCH MAY SPEND MONEY BY ITSELF. The deeper-search lane runs a
/// model over the reader's mail when a query looks like a question the results
/// list cannot answer (docs/SEARCH.md §5), on the user's own key or on the
/// hosted plan's budget. That is spend nobody tapped for, so it is a setting.
///
/// `automatic` is the default because the lane is worth having: the query that
/// motivated the whole design was answered by an agent in one pass and by the
/// search panel not at all.
enum DeeperSearchChoice: String, CaseIterable, Sendable {
    case automatic, onRequest = "on_request", off

    var label: String {
        switch self {
        case .automatic: "Automatic"
        case .onRequest: "On request"
        case .off: "Off"
        }
    }

    /// What the setting promises, in one line, under the picker.
    var blurb: String {
        switch self {
        case .automatic:
            "A question-shaped search reads your mail and answers it, unasked."
        case .onRequest:
            "A question-shaped search offers a button, and waits for you to press it."
        case .off:
            "Search stays keyword and meaning only. No model reads your mail."
        }
    }
}

/// What the panel should DO to the lane, given the setting and where the
/// conversation stands.
enum DeeperSearchPolicy {

    /// One move on the lane. The caller executes it; nothing here touches a
    /// session, which is the point.
    enum Move: Equatable, Sendable {
        /// Leave the lane exactly as it is.
        case nothing
        /// Start the conversation, naming the signal that asked for it.
        case start(trigger: SearchIntent.Trigger)
        /// The same need, narrowed: hand the words to the conversation already
        /// open.
        case refine
        /// Tear the conversation down. `off` is the only thing that says this,
        /// and it says it whether the lane is idle, mid-turn or held.
        case stop
    }

    /// A SETTLED QUERY, already judged (docs/SEARCH.md §5, §6.5).
    ///
    /// Order is the rule. `off` wins over everything, because a preference that
    /// only took effect on queries the classifier liked would leave a running
    /// lane reading mail after the reader said to stop. Then a started lane
    /// takes every settled query as a refinement, INCLUDING one the classifier
    /// would not have started a lane for: somebody who typed a question and
    /// then deleted a word has not stopped asking it, and a lane told only
    /// about the queries that re-triggered it would be answering the question
    /// before last. Only then does a fresh start get considered, and only
    /// `automatic` ever spends unasked: on request, the band offers a button
    /// and nothing runs until it is pressed.
    static func settled(
        verdict: SearchIntent.Verdict, choice: DeeperSearchChoice, laneStarted: Bool
    ) -> Move {
        if choice == .off { return laneStarted ? .stop : .nothing }
        if laneStarted { return .refine }
        guard choice == .automatic, let trigger = verdict.trigger else { return .nothing }
        return .start(trigger: trigger)
    }

    /// THE PICKER ITSELF, MOVED, under a panel that may have a lane running.
    ///
    /// This is what makes `off` mean what its own blurb promises ("No model
    /// reads your mail"). The setting lives on a page the reader has to walk
    /// to, and once they are there they typically never type another query in
    /// the panel they left behind, so waiting for the next settled query to
    /// notice is waiting for something that does not come. The flip itself is
    /// the stopping point.
    static func preferenceChanged(to choice: DeeperSearchChoice, laneStarted: Bool) -> Move {
        choice == .off && laneStarted ? .stop : .nothing
    }
}
