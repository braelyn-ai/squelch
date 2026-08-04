// The per-send read-tracking switch, shared by both composers so the pane and
// the reader's inline reply offer the same thing in the same words.
//
// It renders NOTHING when the daemon has no tracking base_url: `include_tracker`
// is ignored by every send in that case, and a switch that cannot change the
// outcome is worse than no switch. Off is the resting state — the composer seeds
// it from the account default, and the send states whatever it says here.

import SwiftUI

struct TrackerToggle: View {
    @Binding var on: Bool

    @Environment(AppStore.self) private var store

    var body: some View {
        if store.trackingAvailable {
            Button { on.toggle() } label: {
                Chip(
                    text: "track opens",
                    tone: on ? Palette.accent : Palette.inkFaintest,
                    symbol: on ? "eye.fill" : "eye.slash",
                    filled: on)
            }
            .buttonStyle(.plain)
            .help(Self.help)
        }
    }

    /// Says what the pixel is and what it is not. "Opened" is a weaker fact than
    /// it sounds — Gmail fetches images through its own proxy, sometimes before
    /// anyone has read anything — and the toggle is where that gets admitted.
    static let help =
        "Adds an invisible 1×1 image to this email, so you can see when it is opened. "
        + "Not proof somebody read it: Gmail loads images through a proxy that can fetch "
        + "the pixel on its own."
}
