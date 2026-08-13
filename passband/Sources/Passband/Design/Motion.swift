// Motion tokens. Two surfaces doing the same thing have to move at the same
// speed, so the durations live here rather than at the call site.

import SwiftUI

enum Motion {
    /// A list scrolling to keep the selection in view. Short enough to read as
    /// the rows sliding under the cursor rather than as an animation.
    static let scrollFollow = Animation.easeOut(duration: 0.12)

    /// A disclosure opening or closing inside a card. Fast enough that the card
    /// growing under the pointer reads as one gesture, not as a re-layout.
    static let disclose = Animation.snappy(duration: 0.18)

    /// A toast or undo chip arriving and leaving. Shared by both shells so the
    /// same action confirms itself at the same speed on either.
    static let toast = Animation.smooth(duration: 0.24)

    /// A swipe rail settling after the finger lifts — open, closed, or snapped
    /// back from a refused pull. Spring, not a curve: the row has momentum from
    /// the drag, and a curve makes it stop dead the instant contact is lost.
    static let railSettle = Animation.spring(response: 0.32, dampingFraction: 0.82)

    /// A triage-deck card leaving after a committed fling, and the next one
    /// arriving under it. Slower than the rail because the card travels the
    /// width of the screen and the eye follows it out.
    static let deckCard = Animation.spring(response: 0.38, dampingFraction: 0.86)

    /// A phone tab's content arriving from the side its tab sits on: travel along
    /// the bar rather than a replacement of what was there.
    static let tabSlide: Animation = .snappy(duration: 0.28)
}
