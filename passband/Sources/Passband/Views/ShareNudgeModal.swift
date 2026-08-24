// THE TWO-WEEK ASK. After a fortnight of actually using Passband, once and only
// once, a small card asks whether there is anyone worth sharing it with.
//
// MODAL, WITH A SCRIM, which is a heavier hand than `UpdateAlert`'s floating
// card and is meant to be: an update is something the app needs and can ask for
// again tomorrow, and this is a favour that gets exactly one chance. Small,
// centred, and either button ends it forever (see `ShareNudge`).
//
// NO `.defaultAction` ON THE PRIMARY BUTTON, and this is a rule with a scar
// behind it: `KeyDispatch`'s monitor only swallows Return on a surface that
// bound Enter, so a default action on an UNBIDDEN card turns whatever Return
// the user was about to press into a press of this one. A card nobody asked for
// does not get to own the Return key.

import SwiftUI

struct ShareNudgeModal: View {
    @Environment(AppStore.self) private var store
    private var nudge: ShareNudge { ShareNudge.shared }

    var body: some View {
        if nudge.showingNudge {
            ZStack {
                // The scrim. Tappable, and tapping it is "not now": a modal with
                // no way out but a button is a modal people learn to resent.
                Rectangle()
                    .fill(.black.opacity(0.28))
                    .ignoresSafeArea()
                    .onTapGesture { dismiss() }

                card
            }
            .transition(.opacity)
            .animation(Motion.toast, value: nudge.showingNudge)
        }
    }

    private var card: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("You've been using Passband for 2 weeks!")
                .font(.system(size: 14, weight: .semibold))
                .foregroundStyle(Palette.ink)
                .fixedSize(horizontal: false, vertical: true)

            Text(
                "It would help us tremendously if you have any friends you think would also find "
                    + "it useful."
            )
            .font(Typo.rowSub)
            .foregroundStyle(Palette.inkDim)
            .fixedSize(horizontal: false, vertical: true)

            HStack(spacing: 10) {
                Spacer()
                Button("Not now") { dismiss() }
                    .buttonStyle(.glass)
                    .font(.system(size: 12, weight: .medium))
                Button("Share Passband") { share() }
                    .buttonStyle(.glassProminent)
                    .font(.system(size: 12, weight: .semibold))
            }
        }
        .padding(18)
        .frame(maxWidth: 380)
        .passbandGlass(.pane, cornerRadius: 14, tint: Palette.glassTintStrong)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Share Passband with friends")
    }

    private func dismiss() {
        nudge.showingNudge = false
        Analytics.capture("invite_nudge_dismissed")
    }

    private func share() {
        nudge.showingNudge = false
        store.shareSheetOpen = true
        Analytics.capture("invite_nudge_accepted")
    }
}
