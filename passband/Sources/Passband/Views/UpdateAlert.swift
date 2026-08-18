// The new-version card. Floats over whatever page is on screen, bottom centre,
// and says one thing: there is a newer Passband, press this to be running it.
//
// Bottom CENTRE is the only free corner: the toast stack owns bottom-left
// (ActionLayer, inset 74 for the rail), Settings stamps its version number
// bottom-right, and the top edge belongs to the top bar and the connection
// banner. Not modal, because a mail app is a thing you are reading and an
// update is never urgent enough to take the window; not a toast either,
// because a toast leaves on a timer and this waits to be answered.

import SwiftUI

struct UpdateAlert: View {
    @ObservedObject private var updater = Updater.shared

    var body: some View {
        Group {
            switch updater.phase {
            case .idle:
                EmptyView()
            case .checking:
                narration(text: "checking for updates", fraction: nil, spinner: true)
            case .available(let version):
                offer(version: version)
            case .downloading(let fraction):
                narration(text: "downloading Passband", fraction: fraction, spinner: fraction == nil)
            case .extracting(let fraction):
                narration(text: "unpacking the update", fraction: fraction, spinner: false)
            case .installing:
                narration(text: "restarting into the new version", fraction: nil, spinner: true)
            }
        }
        .animation(Motion.toast, value: updater.phase)
    }

    // MARK: - the offer

    private func offer(version: String) -> some View {
        HStack(spacing: 13) {
            Image(systemName: "arrow.down.circle.fill")
                .font(.system(size: 19, weight: .medium))
                .foregroundStyle(Palette.accent)

            VStack(alignment: .leading, spacing: 2) {
                Text("Passband \(version) is available")
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(Palette.ink)
                // What the button DOES, said before it is pressed: the app
                // quits and comes back, and anyone typing an email deserves to
                // know that before they find out.
                Text("Updating installs it and restarts Passband.")
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.inkFaint)
            }

            Spacer(minLength: 10)

            Button("Later") { updater.later() }
                .buttonStyle(.glass)
                .font(.system(size: 12, weight: .medium))

            Button("Update") { updater.installAndRelaunch() }
                .buttonStyle(.glassProminent)
                .font(.system(size: 12, weight: .semibold))
                .keyboardShortcut(.defaultAction)
        }
        .padding(.horizontal, 15)
        .padding(.vertical, 12)
        .frame(maxWidth: 520)
        .passbandGlass(.pane, cornerRadius: 14, tint: Palette.glassTintStrong)
        .transition(.move(edge: .bottom).combined(with: .opacity))
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Passband \(version) is available")
    }

    // MARK: - the work

    /// Everything after the button: one line and, when there is an honest
    /// fraction to draw, a bar. No cancel — Sparkle's own cancellation stops
    /// only the download, and a card offering to abandon a half-installed app
    /// would be offering something it cannot do.
    private func narration(text: String, fraction: Double?, spinner: Bool) -> some View {
        HStack(spacing: 11) {
            if spinner {
                ProgressView()
                    .controlSize(.small)
                    .scaleEffect(0.8)
            } else {
                Image(systemName: "arrow.down.circle")
                    .font(.system(size: 15, weight: .medium))
                    .foregroundStyle(Palette.accent)
            }

            Text(text)
                .font(Typo.rowSub)
                .foregroundStyle(Palette.inkDim)

            if let fraction {
                ProgressView(value: fraction)
                    .progressViewStyle(.linear)
                    .tint(Palette.accent)
                    .frame(width: 110)
                Text("\(Int(fraction * 100))%")
                    .font(Typo.num(11))
                    .foregroundStyle(Palette.inkFaint)
                    .monospacedDigit()
                    .frame(width: 32, alignment: .trailing)
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 9)
        .passbandGlass(.chrome, cornerRadius: 12, tint: Palette.glassTint)
        .transition(.move(edge: .bottom).combined(with: .opacity))
        .accessibilityElement(children: .combine)
        .accessibilityLabel(text)
    }
}
