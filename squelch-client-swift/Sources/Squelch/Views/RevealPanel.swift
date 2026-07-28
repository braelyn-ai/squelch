// Sealed reveal panel. Fetches ONE sealed body on mount (audited + no-store
// server-side), holds it in this view's state ONLY, and drops it on dismiss.
// Never written to disk, never logged, never lifted into the global store.
//
// HTML mail renders through the SAME hard-sandboxed EmailWebView as normal mail
// (script-less, CSP-gated, trackers stripped). No cacheKey is passed: nothing
// about sealed mail is remembered, not even its measured height.
//
// Ported from squelch-desktop/src/components/RevealPanel.tsx.

import SwiftUI

struct RevealPanel: View {
    let meta: SealedMeta
    let onClose: () -> Void

    @State private var revealed: RevealedSealed?
    @State private var error: String?
    @State private var loading = true

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
                        if loading {
                            Text("revealing…")
                                .font(Typo.rowSub).foregroundStyle(Palette.inkFaintest)
                        } else if let error {
                            Text(error).font(Typo.rowSub).foregroundStyle(Palette.danger)
                        } else if let revealed {
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
            .frame(width: 720)
            .squelchGlass(.pane, cornerRadius: 20, tint: Palette.lockSoft.opacity(0.85))
            .shadow(color: .black.opacity(0.32), radius: 46, y: 20)
        }
        .keyContext(.modal)
        .keyBindings(.modal, [
            KeyBinding("Escape", "close reveal", allowInInput: true) { onClose() }
        ])
        .task { await load() }
        // Clear the sensitive body when this view goes away.
        .onDisappear { revealed = nil }
    }

    private var banner: some View {
        HStack(spacing: 7) {
            Image(systemName: "lock.fill").font(.system(size: 11))
            Text("sensitive · one-time reveal · not stored")
                .font(Typo.micro)
            Spacer()
            HStack(spacing: 4) {
                Kbd("esc")
                Text("close").font(Typo.micro)
            }
        }
        .foregroundStyle(Palette.lock)
        .padding(.horizontal, 16)
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity)
        .background(Palette.lockSoft)
    }

    private var fromLine: String {
        let name = revealed?.from_name.map { "\($0) · " } ?? ""
        return "\(name)\(meta.sender) · \(Fmt.dateTime(meta.received_at))"
    }

    private func load() async {
        loading = true
        error = nil
        do {
            revealed = try await APIClient.shared.revealSealed(meta.id)
        } catch {
            self.error = errText(error, "reveal failed")
        }
        loading = false
    }
}
