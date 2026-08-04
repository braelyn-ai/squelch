// Daemon-link health in two deliberately distinct states, so a dead daemon can
// never masquerade as inbox zero: DaemonDownPane replaces the routed view when
// nothing ever loaded this session (empty bands would lie), ConnectionBanner
// keeps already-synced data on screen behind a staleness note. A 401 ("token
// rejected") reads apart from a transport failure; neither echoes URL or token.

import SwiftUI

struct DaemonDownPane: View {
    @Environment(AppStore.self) private var store

    var body: some View {
        if let error = store.refreshError {
            let auth = error.isAuthFailure
            VStack(spacing: 14) {
                Image(systemName: auth ? "key.slash" : "bolt.horizontal.circle")
                    .font(.system(size: 30, weight: .light))
                    .foregroundStyle(auth ? Palette.lock : Palette.warn)

                Text(auth ? "token rejected" : "can't reach the squelch daemon")
                    .font(Typo.serif(26, weight: .medium))
                    .foregroundStyle(Palette.ink)

                Text(
                    auth
                        ? "The server refused the saved token. Update it in Settings."
                        : "The server URL didn't answer. Is squelchd running? Retrying every 10 seconds."
                )
                .font(.system(size: 13))
                .foregroundStyle(Palette.inkFaint)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 380)

                HStack(spacing: 10) {
                    if !auth { RetryButton() }
                    Button {
                        store.setView(.settings)
                    } label: {
                        Label("open settings", systemImage: "gearshape")
                            .font(.system(size: 12, weight: .medium))
                    }
                    .buttonStyle(.glass)
                }
                .padding(.top, 4)
            }
            .padding(38)
            .squelchGlass(.pane, cornerRadius: 22, tint: auth ? Palette.lockSoft : Palette.warnSoft)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }
}

struct ConnectionBanner: View {
    @Environment(AppStore.self) private var store

    var body: some View {
        // Nothing while healthy, or before any successful sync — that failure
        // belongs to DaemonDownPane.
        if let error = store.refreshError, let last = store.lastRefresh {
            let auth = error.isAuthFailure
            let age = Fmt.relAge(last)
            let staleNote = (!age.isEmpty && age != "now") ? " — showing mail from \(age) ago" : ""

            HStack(spacing: 9) {
                Image(systemName: auth ? "key.slash" : "bolt.horizontal.circle")
                    .font(.system(size: 12, weight: .semibold))
                Text(
                    auth ? "token rejected\(staleNote)" : "connection to daemon lost\(staleNote)"
                )
                .font(.system(size: 12, weight: .medium))
                Spacer(minLength: 8)
                if auth {
                    Button("open settings") { store.setView(.settings) }
                        .buttonStyle(.plain)
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundStyle(Palette.accent)
                } else {
                    RetryButton(compact: true)
                }
            }
            .foregroundStyle(auth ? Palette.lock : Palette.warn)
            .padding(.horizontal, 16)
            .padding(.vertical, 8)
            .frame(maxWidth: .infinity)
            .background((auth ? Palette.lockSoft : Palette.warnSoft).opacity(0.9))
            .overlay(alignment: .bottom) { Hairline() }
            .transition(.move(edge: .top).combined(with: .opacity))
        }
    }
}

private struct RetryButton: View {
    var compact = false
    @State private var busy = false

    var body: some View {
        Button {
            busy = true
            Task {
                await SitrepPoller.shared.pull()
                busy = false
            }
        } label: {
            Label("retry now", systemImage: "arrow.clockwise")
                .font(.system(size: compact ? 11 : 12, weight: .medium))
                .symbolEffect(.rotate, isActive: busy)
        }
        .buttonStyle(compact ? AnyButtonStyle(.plain) : AnyButtonStyle(.glass))
        .foregroundStyle(compact ? Palette.warn : Palette.ink)
        .disabled(busy)
    }
}

/// Erase a button style so a view can pick between two at runtime.
struct AnyButtonStyle: PrimitiveButtonStyle {
    private let makeBodyClosure: (Configuration) -> AnyView

    init<S: PrimitiveButtonStyle>(_ style: S) {
        makeBodyClosure = { config in AnyView(style.makeBody(configuration: config)) }
    }

    func makeBody(configuration: Configuration) -> some View {
        makeBodyClosure(configuration)
    }
}
