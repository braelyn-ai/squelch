// The process page: where triage's work gets peer-reviewed — which tier each
// message landed in, and why it landed there. Today it is a door and a frame:
// the all-mail header's peer-review chip needs somewhere real to point, and the
// review tooling grows HERE rather than as another palette or debug overlay.
// Deliberately off the rail and out of the 1..5 keys until it earns a place.

import SwiftUI

struct ProcessView: View {
    var body: some View {
        VStack(spacing: 10) {
            Image(systemName: "checkmark.seal")
                .font(.system(size: 26, weight: .light))
                .foregroundStyle(Palette.inkFaintest)
            Text("Peer review")
                .font(Typo.serif(17, weight: .medium))
                .foregroundStyle(Palette.inkDim)
            Text(
                "Every verdict triage reached, and why it reached it, will be reviewable here."
            )
            .font(Typo.rowSub)
            .foregroundStyle(Palette.inkFaintest)
            .multilineTextAlignment(.center)
            .frame(maxWidth: 340)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}
