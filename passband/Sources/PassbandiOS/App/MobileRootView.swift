// The phone shell. Same three-state gate as the Mac's RootView — loading until
// the keychain answers, the Connect gate until there is an identity, the shell
// after — and the same connect-time lifecycle: poller up, event stream up, the
// inbox and the tracking config warmed.
//
// What differs is only the shape. A Mac gets a rail and a routed page; a phone
// gets a tab bar, which is a different navigation model rather than a smaller
// one, so the rail's seven destinations collapse to four and the rest arrive as
// the tabs that own them grow up.
//
// PROOF OF LIFE, not the product: the Sitrep and Mail tabs render real store
// state (band counts, the day's updates, the inbox page) so a connected phone
// visibly holds live data. The rows are not the Mac's UpdateRow and are not
// pretending to be — the shared row chrome comes over with the reader.

import SwiftUI

/// The four phone destinations. Sitrep is the landing surface, matching the Mac.
private enum MobileTab: Hashable {
    case sitrep, mail, search, settings
}

struct MobileRootView: View {
    @Environment(AppStore.self) private var store

    var body: some View {
        Group {
            switch store.connStatus {
            case .loading:
                MobileLoadingGate()
            case .connected:
                MobileShell()
            default:
                ConnectView()
            }
        }
        // The canvas the glass refracts. On the Mac this is the window backdrop
        // behind the whole scene; a phone has no window to make translucent, so
        // the palette paints it directly.
        .background(Palette.canvas.ignoresSafeArea())
        .task {
            // No EmailWebView.warmProcess() twin: the reader is not on the phone
            // yet, so there is no WebKit process worth paying for at boot.
            await store.loadSettings()
        }
        .onChange(of: store.connStatus) { _, status in
            if status == .connected {
                SitrepPoller.shared.start()
                EventStream.shared.start()
                Task { await store.refreshMail(.inbox) }
                Task { await store.refreshTrackingConfig() }
            } else {
                SitrepPoller.shared.stop()
                EventStream.shared.stop()
            }
        }
    }
}

private struct MobileLoadingGate: View {
    var body: some View {
        VStack(spacing: 14) {
            Text("passband")
                .font(Typo.serif(34, weight: .medium))
                .foregroundStyle(Palette.ink)
            ProgressView()
                .controlSize(.small)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

// MARK: - the tab shell

private struct MobileShell: View {
    @State private var tab: MobileTab = .sitrep

    var body: some View {
        TabView(selection: $tab) {
            Tab("Sitrep", systemImage: MainView.sitrep.symbol, value: MobileTab.sitrep) {
                NavigationStack { SitrepTab() }
            }
            Tab("Mail", systemImage: MainView.emails.symbol, value: MobileTab.mail) {
                NavigationStack { MailTab() }
            }
            Tab("Settings", systemImage: MainView.settings.symbol, value: MobileTab.settings) {
                NavigationStack { PlaceholderTab(
                    title: "Settings",
                    symbol: MainView.settings.symbol,
                    line: "Connection, theme, signature and telemetry move over with the settings pane.")
                }
            }
            // The search role parks this tab apart from the others in iOS 26's
            // tab bar, next to the minimize affordance, which is where a phone
            // user reaches for it.
            Tab(value: MobileTab.search, role: .search) {
                NavigationStack { PlaceholderTab(
                    title: "Search",
                    symbol: "magnifyingglass",
                    line: "Full-text search over the mail the daemon has ingested.")
                }
            }
        }
        // Scrolling down surrenders the bar to the content and brings it back on
        // the way up: a mail list is a reading surface first.
        .tabBarMinimizeBehavior(.onScrollDown)
    }
}

// MARK: - sitrep

/// Band counts plus the updates behind them, straight off `store.sitrep`. This
/// is the surface that proves the SSE feed and the poller are both alive: the
/// numbers move without a gesture.
private struct SitrepTab: View {
    @Environment(AppStore.self) private var store

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                bandRow
                UpdateSection(title: "standing", updates: store.sitrep.standing)
                UpdateSection(title: "new", updates: store.sitrep.new)
                UpdateSection(title: "open", updates: store.sitrep.open)
                if store.lastRefresh == nil && store.refreshError == nil {
                    HStack(spacing: 8) {
                        ProgressView().controlSize(.small)
                        Text("waiting for the first sync…")
                            .font(Typo.rowSub)
                            .foregroundStyle(Palette.inkFaint)
                    }
                    .padding(.vertical, 8)
                }
                if let refreshError = store.refreshError {
                    Label(refreshError.message, systemImage: "exclamationmark.triangle.fill")
                        .font(Typo.rowSub)
                        .foregroundStyle(Palette.danger)
                }
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 24)
        }
        .background(Palette.canvas)
        .navigationTitle("Sitrep")
        .navigationBarTitleDisplayMode(.large)
        .refreshable { await SitrepPoller.shared.pull() }
    }

    /// The three bands as one row of counts. `nil` stats means the first stats
    /// call has not landed — an em-dash, never a zero, because zero is a claim.
    private var bandRow: some View {
        HStack(spacing: 10) {
            BandTile(label: "standing", count: store.sitrep.stats?.bands.standing)
            BandTile(label: "new", count: store.sitrep.stats?.bands.new)
            BandTile(label: "open", count: store.sitrep.stats?.bands.open)
        }
    }
}

private struct BandTile: View {
    let label: String
    let count: Int?

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(count.map(String.init) ?? "—")
                .font(Typo.num(26, weight: .medium))
                .foregroundStyle(Palette.ink)
            Text(label)
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaint)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .zonePadding()
        .passbandGlass(.pane, cornerRadius: 16)
    }
}

private struct UpdateSection: View {
    let title: String
    let updates: [AttentionUpdate]

    var body: some View {
        if !updates.isEmpty {
            VStack(alignment: .leading, spacing: 0) {
                Text(title)
                    .font(Typo.sectionLabel)
                    .foregroundStyle(Palette.inkFaint)
                    .textCase(.uppercase)
                    .padding(.bottom, 8)
                VStack(spacing: 0) {
                    ForEach(updates) { update in
                        MobileUpdateRow(update: update)
                        if update.id != updates.last?.id { Hairline() }
                    }
                }
                .zonePadding()
                .passbandGlass(.pane, cornerRadius: 18)
            }
        }
    }
}

// MARK: - mail

private struct MailTab: View {
    @Environment(AppStore.self) private var store

    private var page: Loadable<[AttentionUpdate]> { store.mailPage(.inbox) }

    var body: some View {
        List {
            if let rows = page.value, !rows.isEmpty {
                ForEach(rows) { update in
                    MobileUpdateRow(update: update)
                        .listRowBackground(Color.clear)
                        .listRowSeparatorTint(Palette.hairline)
                }
            } else if page.isLoading {
                Text("loading mail…")
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.inkFaint)
                    .listRowBackground(Color.clear)
            } else if let error = page.error {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.danger)
                    .listRowBackground(Color.clear)
            } else {
                Text("nothing in the inbox.")
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.inkFaint)
                    .listRowBackground(Color.clear)
            }
        }
        .listStyle(.plain)
        .scrollContentBackground(.hidden)
        .background(Palette.canvas)
        .navigationTitle("Mail")
        .navigationBarTitleDisplayMode(.large)
        .refreshable { await store.refreshMail(.inbox, force: true) }
    }
}

/// One update, phone-sized. Sender, subject line, tier dot — no verbs, because
/// the phone has no undo surface yet and an action you cannot take back is not
/// an action to ship on a walking skeleton.
private struct MobileUpdateRow: View {
    let update: AttentionUpdate

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Circle()
                .fill(Palette.tierColor(update.tier))
                .frame(width: 7, height: 7)
                .padding(.top, 5)
            VStack(alignment: .leading, spacing: 2) {
                Text(SenderCache.resolved(update.senderString).displayName)
                    .font(Typo.row.weight(.medium))
                    .foregroundStyle(Palette.ink)
                    .lineLimit(1)
                Text(update.one_line)
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.inkDim)
                    .lineLimit(2)
            }
            Spacer(minLength: 0)
        }
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

// MARK: - placeholders

/// An honest stub: says what belongs here and does not pretend to hold it.
private struct PlaceholderTab: View {
    let title: String
    let symbol: String
    let line: String

    var body: some View {
        VStack(spacing: 10) {
            Image(systemName: symbol)
                .font(.system(size: 30, weight: .light))
                .foregroundStyle(Palette.inkFaintest)
            Text(title.lowercased())
                .font(Typo.serif(24, weight: .medium))
                .foregroundStyle(Palette.ink)
            Text(line)
                .font(Typo.rowSub)
                .foregroundStyle(Palette.inkFaint)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: 300)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Palette.canvas)
        .navigationTitle(title)
        .navigationBarTitleDisplayMode(.inline)
    }
}
