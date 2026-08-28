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
// ACCOUNT SWITCHING LANDED HERE, above the panes — which is why the Account row
// already sat first rather than last in the Mac's order. The Mac switches worlds
// with ⌘1..⌘9 and a menu on the rail; a phone has neither a chord nor a rail, so
// the list of mailboxes IS the control, and it sits at the top of the one screen
// entered by tapping a person.
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

    private var accounts: [AccountRecord] { AccountManager.shared.accounts }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                masthead
                switcher
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

    /// WHICH MAILBOX THE PANES BELOW ARE ABOUT, and the tap that changes it.
    ///
    /// It renders for a single account too. One row is not a switcher, but it is
    /// still the answer to "who am I signed in as" — and a control that appears
    /// only once a second account exists is one nobody finds before they need
    /// it. Adding is NOT here: it lives in the Account pane, one row down, where
    /// removing and renaming already live. This surface answers which, not how
    /// many.
    private var switcher: some View {
        VStack(spacing: 0) {
            ForEach(accounts) { account in
                let isActive = account.id == AccountManager.shared.activeId
                Button {
                    // `switchTo` declines the account already live, so a tap on
                    // the checked row costs nothing — no flush, no epoch bump,
                    // no world rebuilt to arrive where it started. Which is why
                    // the row is not `.disabled`: dimming the ACTIVE account
                    // would say the opposite of what the checkmark says.
                    Task { await AccountManager.shared.switchTo(account.id) }
                } label: {
                    accountRow(account, isActive: isActive)
                }
                .buttonStyle(.plain)
                if account.id != accounts.last?.id { Hairline() }
            }
        }
        .padding(.vertical, 4)
        .frame(maxWidth: .infinity, alignment: .leading)
        .passbandGlass(.pane, cornerRadius: 18, tint: Palette.glassTint)
    }

    private func accountRow(_ account: AccountRecord, isActive: Bool) -> some View {
        HStack(spacing: 12) {
            // The same initial-in-a-circle the Mac's rail wears, so one person
            // on both devices recognizes an account by the same mark.
            Text(account.initial)
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(Palette.accent)
                .frame(width: 26, height: 26)
                .background(Circle().fill(Palette.accentSoft))
            VStack(alignment: .leading, spacing: 2) {
                Text(account.displayName)
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.ink)
                    .lineLimit(1)
                // Only when it would SAY something new: `displayName` already
                // falls back to the host, and a row reading "baddiebox:8848"
                // twice is noise dressed as detail.
                if let host = subtitle(account) {
                    Text(host)
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkFaintest)
                        .lineLimit(1)
                }
            }
            Spacer(minLength: 8)
            if isActive {
                Image(systemName: "checkmark")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Palette.accent)
                    .accessibilityLabel("Active account")
            }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 11)
        .contentShape(Rectangle())
    }

    /// The daemon behind a NAMED account. Nil when the name is already the host
    /// (or when the host is not known yet, which is a record written before the
    /// field existed — `noteDisplayHost` fills those in at boot).
    private func subtitle(_ account: AccountRecord) -> String? {
        let named = account.label.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !named.isEmpty, !account.displayHost.isEmpty else { return nil }
        return account.displayHost
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
                    // NO TourSection, and NO WhatsNewSection, for the same
                    // reason: both render through the desktop's ActionLayer,
                    // which has no iOS host yet. Either button would set state
                    // (`tour.active`, `whatsNew.notes`) that no surface here is
                    // able to show or dismiss. The phone's what's-new is the
                    // App Store's release notes until that host exists.
                    DeveloperSection()
                    YouSection()
                case .mail:
                    MailSection()
                    SearchSection()
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
        case .mail: "images, search order, signature, read tracking"
        case .triage: "how it works, daily caps, ranking"
        case .assistant: "your own api key, and which model"
        case .privacy: "what telemetry leaves the app"
        case .account: "the mailboxes this install knows"
        }
    }
}

/// THE SEARCH ORDER, on the phone. The Mac hangs the same control in the
/// settings header, top right beside the title (SettingsView), where it is
/// reachable from every tab. This screen has no header to hang it from, so it
/// becomes a card like every other preference here — filed under Mail, next to
/// the rest of what searching turns up.
///
/// Lives in the iOS shell rather than beside the shared section cards because
/// it is PACKAGING, not the setting: the control and the preference are both
/// shared, and only the frame around them is a phone decision.
struct SearchSection: View {
    @Environment(Prefs.self) private var prefs

    var body: some View {
        SectionCard(label: "Search") {
            InlineRow(key: "order") { SearchSortPicker() }
            SettingsHint(
                "Recent ranks newer mail higher when two matches are close, which is usually the one you meant. Best match ignores the date and ranks on the words alone, for a thread you can quote but cannot place. Either way the search itself is unchanged: this is the order results come back in, not which mail is found."
            )
        }
    }
}
