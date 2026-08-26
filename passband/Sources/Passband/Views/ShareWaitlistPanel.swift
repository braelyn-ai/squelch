// SHARE PASSBAND, WITH NOTHING TO HAND OUT YET.
//
// The OAuth client is not verified (`docs/VERIFICATION.md`), so a Google
// account that is not on the project's test-user list cannot finish consent.
// An invite code mailed to somebody in that position is spent on a wall: they
// click it, Google refuses them, and the code is gone. Until verification
// clears there is exactly one honest thing to hand a friend, and it is the
// waitlist.
//
// SO THIS REPLACES `SharePanel` RATHER THAN CHANGING IT. That screen — the
// composer, the daemon's draft, the `{{invite}}` marker, the recipient pills —
// is built and tested and correct, and it is what should be on screen the day
// the review clears. Deleting it to re-derive it later would be the expensive
// kind of tidy. `ShareGate` is the one line that swaps them back.
//
// NO ANALYTICS EVENT HERE, deliberately. `Analytics.allowedEvents` is a CLOSED
// set that fatally asserts in debug on an unknown name, and a temporary screen
// is not worth widening the app's permanent vocabulary for.

import SwiftUI

/// Whether the client can mint and mail invite codes.
///
/// ONE CONSTANT, because the answer is about Google's review queue and not
/// about any user, daemon, or build. Flip it to `true` when verification clears
/// and `SharePanel` comes back on both platforms at once.
enum ShareGate {
    static let invitesEnabled = false
}

struct ShareWaitlistPanel: View {
    @Environment(\.dismiss) private var dismiss

    /// Set by the copy button, and never unset: the label it changes to IS the
    /// receipt, and a message that erases itself after a beat is one the user
    /// can miss by blinking.
    @State private var copied = false

    private static let waitlist = "https://passband.app/waitlist"

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider().overlay(Palette.hairline)
            content
        }
        // `maxWidth` rather than `width`, because this one sheet is presented
        // on a phone as well as in a Mac window and a hard 560 has nowhere to
        // go on the narrow one.
        .frame(maxWidth: 560)
        .background(Palette.canvas)
    }

    private var header: some View {
        HStack(alignment: .firstTextBaseline) {
            Text("Share Passband")
                .font(Typo.serif(19))
                .foregroundStyle(Palette.ink)
            Spacer()
            Button("Done") { dismiss() }
                .buttonStyle(.glass)
                .font(.system(size: 12, weight: .medium))
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 16)
    }

    private var content: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Passband is invite-only right now.")
                .font(Typo.row)
                .foregroundStyle(Palette.ink)

            // WHY, not just what. "You cannot do this" with no reason reads as
            // a bug in the app; the real reason is short, true, and ends.
            Text(
                "We are waiting on Google to verify Passband's Gmail access, and until that "
                    + "clears we cannot hand out invite codes. Send anyone you think would like "
                    + "it to the waitlist instead and we will let them in from there."
            )
            .font(Typo.rowSub)
            .foregroundStyle(Palette.inkDim)
            .fixedSize(horizontal: false, vertical: true)

            Text(Self.waitlist)
                .font(Typo.rowSub.monospaced())
                .foregroundStyle(Palette.ink)
                .textSelection(.enabled)
                .padding(.vertical, 2)

            HStack(spacing: 10) {
                Button(copied ? "Copied" : "Copy link") { copy() }
                    .buttonStyle(.glass)
                    .font(.system(size: 12, weight: .medium))
                Button("Open the waitlist") { openWaitlist() }
                    .buttonStyle(.glassProminent)
                    .font(.system(size: 12, weight: .semibold))
                Spacer()
            }
        }
        .padding(20)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func copy() {
        // The button reports what the OS actually did rather than what was
        // asked, because a pasteboard that refused and a label that says
        // "Copied" is how somebody pastes nothing into a message.
        copied = Platform.copyToPasteboard(Self.waitlist)
    }

    private func openWaitlist() {
        guard let url = URL(string: Self.waitlist) else { return }
        _ = Platform.open(url)
    }
}
