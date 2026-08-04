// First-run connect screen: server URL + API token, tested via /client/stats and
// saved into the OS keychain. The token is never logged.

import SwiftUI

struct ConnectView: View {
    @Environment(AppStore.self) private var store

    @State private var url = "http://127.0.0.1:8848"
    @State private var token = ""
    @FocusState private var tokenFocused: Bool

    private var busy: Bool { store.connStatus == .connecting }

    var body: some View {
        ZStack {
            VStack(spacing: 0) {
                VStack(alignment: .leading, spacing: 4) {
                    Text("passband")
                        .font(Typo.serif(40, weight: .medium))
                        .foregroundStyle(Palette.ink)
                    Text("connect to your human door")
                        .font(.system(size: 13))
                        .foregroundStyle(Palette.inkFaint)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.bottom, 22)

                VStack(alignment: .leading, spacing: 14) {
                    Field(label: "server url") {
                        TextField("http://127.0.0.1:8848", text: $url)
                            .textFieldStyle(.plain)
                            .textContentType(.URL)
                            .autocorrectionDisabled()
                            .onSubmit { tokenFocused = true }
                    }
                    Field(label: "api token") {
                        SecureField("SQUELCH_API_TOKEN", text: $token)
                            .textFieldStyle(.plain)
                            .focused($tokenFocused)
                            .onSubmit { submit() }
                    }
                }

                if let error = store.connError {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .font(.system(size: 12))
                        .foregroundStyle(Palette.danger)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.top, 12)
                }

                Button {
                    submit()
                } label: {
                    Text(busy ? "testing…" : "connect")
                        .font(.system(size: 13, weight: .semibold))
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 5)
                }
                .buttonStyle(.glassProminent)
                .tint(Palette.accent)
                .disabled(busy || url.trimmed.isEmpty || token.trimmed.isEmpty)
                .padding(.top, 18)

                Text("The token is stored in your macOS keychain and sent only as a bearer header.")
                    .font(.system(size: 11))
                    .foregroundStyle(Palette.inkFaintest)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.top, 14)
            }
            .padding(30)
            .frame(width: 440)
            .passbandGlass(.pane, cornerRadius: 24, tint: Palette.glassTintStrong)
            .shadow(color: .black.opacity(0.3), radius: 50, y: 24)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func submit() {
        guard !busy, !url.trimmed.isEmpty, !token.trimmed.isEmpty else { return }
        Task { await store.connect(serverURL: url.trimmed, apiToken: token.trimmed) }
    }
}

/// A labelled input well. Near-opaque on purpose: a fully translucent field
/// over a busy wallpaper leaves typed text unreadable.
struct Field<Content: View>: View {
    let label: String
    @ViewBuilder var content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            FieldLabel(label)
            content.fieldWell()
        }
    }
}

/// The caption above a well. Split out so a field needing its own row under the
/// label still labels itself identically.
struct FieldLabel: View {
    let text: String
    init(_ text: String) { self.text = text }

    var body: some View {
        Text(text)
            .font(Typo.micro)
            .foregroundStyle(Palette.inkFaint)
            .textCase(.lowercase)
    }
}

extension View {
    func fieldWell() -> some View {
        self
            .font(.system(size: 13))
            .foregroundStyle(Palette.ink)
            .padding(.horizontal, 10)
            .padding(.vertical, 8)
            .background(
                RoundedRectangle(cornerRadius: 9, style: .continuous)
                    .fill(Palette.canvas.opacity(0.65))
            )
            .overlay(
                RoundedRectangle(cornerRadius: 9, style: .continuous)
                    .strokeBorder(Palette.hairlineStrong, lineWidth: 0.75)
            )
    }
}

extension String {
    var trimmed: String { trimmingCharacters(in: .whitespacesAndNewlines) }
}
