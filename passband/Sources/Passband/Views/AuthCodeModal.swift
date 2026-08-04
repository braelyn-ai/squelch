// The "present, don't read" payoff: an arriving otp/verification message is
// auto-revealed (audited) and its code shown big, so the human copies it without
// reading the mail. The code lives in store state only, never persisted, and is
// dropped from the queue on dismiss. Auto-dismisses after 30s so a secret never
// sits on screen; the timer is per code, and copying pauses it.

import SwiftUI

struct AuthCodeModal: View {
    @Environment(AppStore.self) private var store

    private static let autoDismissSeconds = 30

    @State private var copied = false
    @State private var secondsLeft = autoDismissSeconds
    @State private var paused = false

    private var entry: AuthCodeEntry? { store.authQueue.first }
    private var code: String? { entry?.code }

    var body: some View {
        if let entry {
            OverlayScrim(onDismiss: { store.dismissAuthCode() }) {
                VStack(alignment: .leading, spacing: 0) {
                    header(entry)

                    if let code, !code.isEmpty {
                        Text(code)
                            .font(Typo.mono(46, weight: .semibold))
                            .foregroundStyle(Palette.ink)
                            .tracking(6)
                            .frame(maxWidth: .infinity)
                            .padding(.vertical, 22)
                            .accessibilityLabel("login code")
                    } else {
                        Text(
                            "couldn't read a code from this one — open Auth to reveal it yourself."
                        )
                        .font(Typo.rowSub)
                        .foregroundStyle(Palette.inkFaint)
                        .multilineTextAlignment(.center)
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 20)
                    }

                    actions

                    HStack(spacing: 4) {
                        Text("not stored · revealing it was audited")
                            .font(Typo.micro)
                            .foregroundStyle(Palette.inkFaintest)
                        if !paused && secondsLeft > 0 {
                            Text("· \(secondsLeft)s")
                                .font(Typo.num(10))
                                .foregroundStyle(Palette.inkFaintest)
                                .help("auto-dismisses; copying keeps it open")
                        }
                        Spacer()
                    }
                    .padding(.horizontal, 18)
                    .padding(.bottom, 10)

                    // Draining countdown bar; freezes green once copied.
                    GeometryReader { geo in
                        Rectangle()
                            .fill(paused ? Palette.positive : Palette.lock)
                            .frame(
                                width: geo.size.width
                                    * CGFloat(max(0, secondsLeft)) / CGFloat(Self.autoDismissSeconds)
                            )
                            .animation(.linear(duration: 1), value: secondsLeft)
                    }
                    .frame(height: 2)
                }
                .frame(width: 420)
                .passbandGlass(.pane, cornerRadius: 22, tint: Palette.lockSoft.opacity(0.9))
                .shadow(color: .black.opacity(0.34), radius: 50, y: 22)
            }
            .keyContext(.modal)
            .keyBindings(.modal, [
                KeyBinding("Escape", "dismiss") { store.dismissAuthCode() },
                KeyBinding("Enter", "dismiss") { store.dismissAuthCode() },
                KeyBinding("c", "copy code") { copy() },
            ])
            .task(id: entry.id) { await runTimer() }
            .onChange(of: entry.id) { _, _ in
                copied = false
                paused = false
            }
        }
    }

    private func header(_ entry: AuthCodeEntry) -> some View {
        HStack(spacing: 10) {
            Avatar(sender: entry.meta.sender, size: 28)
            VStack(alignment: .leading, spacing: 1) {
                Text(SenderCache.resolved(entry.meta.sender).displayName)
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(Palette.ink)
                    .help(entry.meta.sender)
                Label(AuthCopy.label(entry.meta.kind), systemImage: AuthCopy.symbol(entry.meta.kind))
                    .font(Typo.micro)
                    .foregroundStyle(Palette.lock)
            }
            Spacer(minLength: 6)
            if store.authQueue.count > 1 {
                Text("+\(store.authQueue.count - 1)")
                    .font(Typo.num(11, weight: .semibold))
                    .foregroundStyle(Palette.inkFaintest)
                    .padding(.horizontal, 7).padding(.vertical, 2)
                    .background(Capsule().fill(Palette.hairline))
                    .help("more codes waiting")
            }
        }
        .padding(.horizontal, 18)
        .padding(.top, 16)
    }

    private var actions: some View {
        HStack(spacing: 8) {
            if let code, !code.isEmpty {
                Button(action: copy) {
                    HStack(spacing: 6) {
                        Image(systemName: copied ? "checkmark" : "doc.on.doc")
                        Text(copied ? "copied" : "copy")
                        Kbd("c")
                    }
                    .font(.system(size: 12, weight: .medium))
                    .padding(.horizontal, 12).padding(.vertical, 5)
                }
                .buttonStyle(.glassProminent)
                .tint(Palette.lock)
            } else {
                Button {
                    store.dismissAuthCode()
                    store.setView(.auth)
                } label: {
                    Label("open Auth", systemImage: "key.fill")
                        .font(.system(size: 12, weight: .medium))
                        .padding(.horizontal, 12).padding(.vertical, 5)
                }
                .buttonStyle(.glassProminent)
                .tint(Palette.lock)
            }
            Button {
                store.dismissAuthCode()
            } label: {
                HStack(spacing: 6) {
                    Text("dismiss")
                    Kbd("esc")
                }
                .font(.system(size: 12))
                .padding(.horizontal, 10).padding(.vertical, 5)
            }
            .buttonStyle(.glass)
            .foregroundStyle(Palette.inkDim)
            Spacer()
        }
        .padding(.horizontal, 18)
        .padding(.bottom, 12)
    }

    private func copy() {
        guard let code, !code.isEmpty else { return }
        // Copying is the human acting on the code — stop the auto-dismiss clock.
        if Clip.copy(code, flashing: $copied) { paused = true }
    }

    private func runTimer() async {
        paused = false
        secondsLeft = Self.autoDismissSeconds
        while secondsLeft > 0 {
            try? await Task.sleep(for: .seconds(1))
            if Task.isCancelled { return }
            if paused { continue }
            secondsLeft -= 1
        }
        store.dismissAuthCode()
    }
}
