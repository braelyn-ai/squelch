// Touch feedback. A phone confirms a committed gesture in the hand, not on the
// screen: the Mac says "done" with a row leaving and a toast, and a thumb that
// has just flung a card sideways is not looking at either.
//
// ONE generator per call, deliberately. The cached-generator dance
// (prepare/impact/keep-alive) buys latency for a continuously-driven surface —
// a scrubber, a picker wheel — and every gesture here fires once, at the end,
// where the Taptic Engine's cold start is under the animation that follows it.

import UIKit

@MainActor
enum Haptics {
    /// A gesture crossed its commit threshold and the verb fired: a swipe let go
    /// past the rail, a deck card flung out. Light, because the app fires these
    /// during triage runs where every third gesture is one of them.
    static func commit() {
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
    }

    /// A drag reached the point where letting go WOULD commit — the same
    /// threshold `commit()` reports, felt before the finger lifts, so the rail
    /// can be trusted without watching it. Fires once per crossing; the caller
    /// owns that edge detection.
    static func armed() {
        UIImpactFeedbackGenerator(style: .rigid).impactOccurred(intensity: 0.6)
    }
}
