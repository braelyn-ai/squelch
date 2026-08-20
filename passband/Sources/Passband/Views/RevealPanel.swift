// Sealed reveal: fetches one body on mount (audited and no-store server-side),
// holds it in this view's state only, drops it on dismiss — never to disk, never
// logged, never lifted into the store. HTML renders through the same sandboxed
// EmailWebView as normal mail, with no cacheKey: nothing about sealed mail is
// remembered, not even its measured height. No attachments either, and that is
// the answer rather than an omission — a reveal fetches one body and nothing
// else, so its `cid:` images resolve to nothing and drop. See docs/SECURITY.md.

import SwiftUI

struct RevealPanel: View {
    let meta: SealedMeta
    let onClose: () -> Void

    @State private var revealState: Loadable<RevealedSealed> = .loading

    var body: some View {
        OverlayScrim(onDismiss: onClose) {
            VStack(alignment: .leading, spacing: 0) {
                banner

                VStack(alignment: .leading, spacing: 4) {
                    Text(meta.subject)
                        .font(.system(size: 15, weight: .semibold))
                        .foregroundStyle(Palette.ink)
                    HStack(spacing: 5) {
                        Text(fromLine)
                            .font(Typo.micro)
                            .foregroundStyle(Palette.inkFaint)
                        if meta.kind != nil {
                            Chip(
                                text: AuthCopy.label(meta.kind), tone: Palette.lock,
                                symbol: AuthCopy.symbol(meta.kind))
                        }
                    }
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 12)

                Divider().overlay(Palette.hairline)

                ScrollView {
                    Group {
                        if revealState.isLoading {
                            Text("revealing…")
                                .font(Typo.rowSub).foregroundStyle(Palette.inkFaintest)
                        } else if let error = revealState.error {
                            Text(error).font(Typo.rowSub).foregroundStyle(Palette.danger)
                        } else if let revealed = revealState.value {
                            if let html = revealed.html, !html.isEmpty {
                                EmailWebView(html: html)
                            } else {
                                Text(revealed.body)
                                    .font(.system(size: 13))
                                    .foregroundStyle(Palette.ink)
                                    .textSelection(.enabled)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                                    .fixedSize(horizontal: false, vertical: true)
                            }
                        }
                    }
                    .padding(16)
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                .frame(maxHeight: 480)

                HStack {
                    Text("cleared from memory when you close this.")
                    Spacer()
                    Text("audited server-side")
                }
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
                .padding(.horizontal, 16)
                .padding(.vertical, 10)
            }
            // A Mac sizes this against the window it floats in; a phone has one
            // measure and the panel takes it.
            #if os(macOS)
                .frame(width: 720)
            #else
                .frame(maxWidth: .infinity)
                .padding(.horizontal, 12)
            #endif
            .passbandGlass(.pane, cornerRadius: 20, tint: Palette.lockSoft.opacity(0.85))
            .shadow(color: .black.opacity(0.32), radius: 46, y: 20)
        }
        .keyContext(.modal)
        .keyBindings(.modal, [
            KeyBinding("Escape", "close reveal", allowInInput: true) { onClose() }
        ])
        .task { await load() }
        // Clear the sensitive body when this view goes away — `.idle` drops the
        // decoded value, which is the only copy of it.
        .onDisappear { revealState = .idle }
    }

    private var banner: some View {
        HStack(spacing: 7) {
            Image(systemName: "lock.fill").font(.system(size: 11))
            Text("sensitive · one-time reveal · not stored")
                .font(Typo.micro)
            Spacer()
            // The keycap is the Mac's exit. A phone taps the scrim, and a hint
            // naming a key it has no way to press is worse than no hint.
            #if os(macOS)
                HStack(spacing: 4) {
                    Kbd("esc")
                    Text("close").font(Typo.micro)
                }
            #else
                Button("close", action: onClose)
                    .font(Typo.micro)
                    .buttonStyle(.plain)
                    .foregroundStyle(Palette.lock)
            #endif
        }
        .foregroundStyle(Palette.lock)
        .padding(.horizontal, 16)
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity)
        .background(Palette.lockSoft)
    }

    private var fromLine: String {
        let name = revealState.value?.from_name.map { "\($0) · " } ?? ""
        return "\(name)\(meta.sender) · \(Fmt.dateTime(meta.received_at))"
    }

    private func load() async {
        await $revealState.load("reveal failed") {
            try await APIClient.shared.revealSealed(meta.id)
        }
        // How often humans actually need behind the seal — the count and
        // nothing else; the body itself never leaves the view above.
        if revealState.value != nil { Analytics.capture("sealed_revealed") }
    }
}
