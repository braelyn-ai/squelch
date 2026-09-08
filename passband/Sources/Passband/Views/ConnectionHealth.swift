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
            .passbandGlass(.pane, cornerRadius: 22, tint: auth ? Palette.lockSoft : Palette.warnSoft)
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
            // Compact, because the bar is now a third of the window rather than
            // all of it: the long form ("showing mail from 4h ago") truncated
            // before the retry button on any normal window.
            let staleNote = (!age.isEmpty && age != "now") ? " · \(age) old" : ""

            // RIGHT THIRD ONLY. Full width put an alarm-coloured bar across the
            // title row, starting a few points from the traffic lights — the
            // rail is 60 wide and the dots span x 9-69, so the two shared a
            // line and the window read as broken rather than as disconnected.
            // A leading Spacer plus a one-third container frame keeps the row's
            // full height (nothing below it shifts) while the tinted box stays
            // out of the window controls' way.
            HStack(spacing: 0) {
                Spacer(minLength: 0)

                HStack(spacing: 9) {
                    Image(systemName: auth ? "key.slash" : "bolt.horizontal.circle")
                        .font(.system(size: 12, weight: .semibold))
                    Text(
                        auth ? "token rejected\(staleNote)" : "daemon unreachable\(staleNote)"
                    )
                    .font(.system(size: 12, weight: .medium))
                    .lineLimit(1)
                    .truncationMode(.tail)
                    Spacer(minLength: 8)
                    if auth {
                        Button("settings") { store.setView(.settings) }
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
                .background((auth ? Palette.lockSoft : Palette.warnSoft).opacity(0.9))
                .overlay(alignment: .bottom) { Hairline() }
                .containerRelativeFrame(.horizontal, count: 3, span: 1, spacing: 0)
            }
            .frame(maxWidth: .infinity)
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
