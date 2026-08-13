// THE ACCOUNT PAGE — the phone's you-space, behind the person glyph at the top
// left of the sitrep. Everything about the person using the app and the install
// they are using it on lives here: which mailboxes this install knows first, and
// then the six settings panes under that.
//
// WHY THE SETTINGS LIVE UNDER THE PERSON AND NOT BESIDE THE MAIL. A phone settles
// settings behind whoever is signed in — it is where every other app on the
// device has trained a thumb to look, and it is honest about the relationship:
// the switches are the account's, not the inbox's. Spending one of a phone's
// three tabs on a screen you visit twice a month would have been the trade going
// the other way.
//
// ACCOUNT SWITCHING WILL LAND HERE, and nothing below builds it yet. The Mac's
// multi-account shell (one active world, N background ears, ⌘1..⌘9) has no phone
// equivalent, and when it gets one this is the surface it appears on, above the
// panes, which is why the Account row already sits first rather than last in the
// Mac's order.
//
// The panes here hold THE SAME STRUCTS the Mac's Views/SettingsView.swift lays
// out: ConnectionSection, SignatureSection, TriageBudgetSection and the rest are
// one declaration each, composed by whichever shell is on screen. That is the
// whole design. A settings screen is the surface most likely to be reimplemented
// "just for the phone", and the moment it is, the two copies start disagreeing
// about what a toggle writes, which cap is which, and what a hint promises. They
// cannot disagree here, because there is only one of each.
//
// WHAT DIFFERS IS THE NAVIGATION, AND ONLY THAT. A window can put six panes in a
// column beside their contents and let you scan the whole map at once; a phone
// has room for the map or the territory, never both. So the six become a list you
// tap into and back out of, which is also the shape every other settings screen
// on the device has — the one place in this app where matching the platform's
// habit beats having a house style.
//
// A LIST AND NOT A `List`. The app paints its own canvas and floats glass on it,
// and `insetGrouped` brings a grouped system background plus separator insets
// that fight both. This is the same ScrollView-of-cards MobileRecordsView is, and
// the index reads as inset-grouped because the rows share one glass pane.

import SwiftUI

struct AccountPage: View {
    /// The panes, in the order a PHONE wants them: the account first, because
    /// this page is entered through a person icon and the row that answers
    /// "who am I signed in as" should not be the sixth one down. The Mac's
    /// sub-nav keeps its own order — `SettingsSection.allCases` — since a rail
    /// beside the pane is a map, and a map you can read at a glance has no
    /// reason to lead with anything.
    private static let order: [SettingsSection] = [
        .account, .general, .mail, .triage, .assistant, .privacy,
    ]

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                masthead
                index
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 28)
        }
        .background(Palette.canvas)
        .navigationTitle("Account")
        .navigationBarTitleDisplayMode(.inline)
    }

    /// The one serif line on the index, same as Quick Look's: says what the six
    /// rows have in common so the screen reads as a place, not a menu.
    private var masthead: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text("account")
                .font(Typo.micro)
                .foregroundStyle(Palette.accent)
                .textCase(.uppercase)
                .tracking(0.6)
            Text("You, and how this one behaves.")
                .font(Typo.hero(26))
                .foregroundStyle(Palette.ink)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.top, 4)
    }

    /// One glass pane, six rows, hairlines between: the inset-grouped shape,
    /// built out of the app's own material rather than the system's.
    private var index: some View {
        VStack(spacing: 0) {
            ForEach(Self.order, id: \.self) { section in
                NavigationLink { pane(section) } label: { row(section) }
                    .buttonStyle(.plain)
                if section != Self.order.last { Hairline() }
            }
        }
        .padding(.vertical, 4)
        .frame(maxWidth: .infinity, alignment: .leading)
        .passbandGlass(.pane, cornerRadius: 18, tint: Palette.glassTint)
    }

    private func row(_ section: SettingsSection) -> some View {
        HStack(spacing: 12) {
            Image(systemName: symbol(section))
                .font(.system(size: 14, weight: .medium))
                .foregroundStyle(Palette.accent)
                .frame(width: 22)
            VStack(alignment: .leading, spacing: 2) {
                Text(section.label)
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.ink)
                Text(blurb(section))
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                    .lineLimit(1)
            }
            Spacer(minLength: 8)
            Image(systemName: "chevron.right")
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(Palette.inkFaintest)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 11)
        // The whole row is the tap target, not just the glyphs on it.
        .contentShape(Rectangle())
    }

    /// THE PANES, and the one place the mapping lives. Same grouping as the
    /// Mac's sub-nav, because a user who has seen both should find the
    /// read-tracking switch under Mail either way.
    @ViewBuilder private func pane(_ section: SettingsSection) -> some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                switch section {
                case .general:
                    ConnectionSection()
                    AppearanceSection()
                    NotificationsSection()
                    // NO TourSection: the tour renders in TourOverlay, which is
                    // mounted by the desktop's ActionLayer and has no iOS host
                    // yet. The button would set `tour.active` with no surface
                    // able to show or dismiss it.
                    DeveloperSection()
                    YouSection()
                case .mail:
                    MailSection()
                    SignatureSection()
                    ReadTrackingSection()
                case .triage:
                    TriagePipelineSection()
                    TriageBudgetSection()
                    RankingSection()
                case .assistant:
                    AssistantSection()
                case .privacy:
                    PrivacySection()
                case .account:
                    AccountSection()
                }
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 28)
        }
        .background(Palette.canvas)
        .navigationTitle(section.label)
        .navigationBarTitleDisplayMode(.inline)
    }

    /// Icons and one-line blurbs live HERE rather than on `SettingsSection`,
    /// which is shared with the Mac and needs neither: the rail names the pane
    /// and the pane itself is already on screen beside it. A list row has to say
    /// what is behind it before you commit a tap to finding out.
    private func symbol(_ section: SettingsSection) -> String {
        switch section {
        case .general: "gearshape"
        case .mail: "envelope"
        case .triage: "arrow.triangle.branch"
        case .assistant: "sparkles"
        case .privacy: "hand.raised"
        case .account: "person.crop.circle"
        }
    }

    private func blurb(_ section: SettingsSection) -> String {
        switch section {
        case .general: "connection, theme, chime, your name"
        case .mail: "images, signature, read tracking"
        case .triage: "how it works, daily caps, ranking"
        case .assistant: "your own api key, and which model"
        case .privacy: "what telemetry leaves the app"
        case .account: "the mailboxes this install knows"
        }
    }
}
