// THE PHONE'S DASHBOARD — the ACTION half of the Mac's SitrepView, folded to one
// column: the hero states what needs you today, For-your-eyes ranks the standing
// band, and the newsletters zone offers the rule-onboarding it always has.
//
// The Mac's other half, the pinned records rail, is a TAB here instead
// (MobileRecordsView). That split is the phone's own: a Mac shows work surface
// and reference material side by side, and a phone that stacked them would bury
// the four record cards under a scroll nobody reaches — reference material you
// have to scroll past your obligations to reach is reference material you stop
// consulting.
//
// WHAT IS NOT HERE, and why. The Mac's masthead is a chrome bar (wordmark,
// re-triage, today's stamp, sync age) — a phone has a navigation bar and a tab
// bar for all of that. The status strip's doors (rules, cost) belong to
// destinations this phase does not have. And every keyboard cursor the Mac's
// dashboard carries is a hover/j-k concept with no thumb equivalent: a tap
// opens the mail, and that is the whole interaction.
//
// TWO THINGS THIS SCREEN USED TO CARRY AND NO LONGER DOES. The band-count strip
// (standing / new / open) was a dashboard metric on a screen whose whole
// argument is that it names obligations in words, not numbers. And the process
// deck is gone from the phone entirely — a card-at-a-time queue wants a keyboard
// and a session, and neither is what a phone is for. The body below stays one
// thing: what needs you.
//
// WHAT IT GAINED IS TWO CORNERS. The navigation bar now carries the account page
// (leading) and the login codes (trailing), because this is the screen the app
// opens on and neither of those was worth a tab. They are in the CHROME and not
// in the column on purpose: the scroll is obligations, and a bar button is
// reachable without disturbing a single one of them.

import SwiftUI

struct MobileSitrepView: View {
    @Environment(AppStore.self) private var store
    @Environment(Prefs.self) private var prefs

    /// NewslettersZone takes a cursor because the Mac drives it with the
    /// keyboard. Nothing writes to this one — it is the shared component's
    /// price of admission, and holding it here keeps that zone byte-identical
    /// across the two shells rather than forking it for the phone.
    @State private var cursor = SitrepCursor()

    /// How many ranked standing items show before the "{n} more" expander —
    /// four, well short of the Mac's ten, because a phone's fold is shorter and
    /// four ranked rows plus the expander still fit under it with the hero.
    private static let eyesVisible = 4
    @State private var expanded = false

    /// Is there a sealed message young enough to still be worth racing to? The
    /// window and the test both belong to MobileAuthView, which is the surface
    /// that has to keep its word about them — this is only the badge asking.
    private var liveCode: Bool {
        store.sitrep.sealed.contains { MobileAuthView.isLive($0) }
    }

    /// Spoken instead of drawn, for the same dot. Typed as `String` rather than
    /// inlined: a ternary of two literals leaves the compiler choosing between
    /// the StringProtocol and LocalizedStringKey overloads of
    /// `accessibilityValue`.
    private var liveCodeValue: String {
        liveCode ? "a code just arrived" : ""
    }

    var body: some View {
        // ONE rank per render, exactly as the Mac does it: this sorts the whole
        // standing band, and a computed property read three times would sort it
        // three times on every scroll frame.
        let ranked = Ranking.rank(store.sitrep.standing, weight: prefs.rankWeight)
        let visible = expanded ? ranked : Array(ranked.prefix(Self.eyesVisible))
        let overflow = ranked.count - Self.eyesVisible

        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                hero
                if !ranked.isEmpty {
                    forYourEyes(visible: visible, overflow: overflow, queue: ranked)
                }
                NewslettersZone(
                    newsletters: Newsletters.prune(
                        store.zones.newsletters, resolved: store.resolvedIds),
                    cursor: cursor)
                footnote
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 28)
        }
        .background(Palette.canvas)
        .navigationTitle("Sitrep")
        .navigationBarTitleDisplayMode(.inline)
        // THE TWO DOORS OFF THE DASHBOARD. Both of these used to be tabs, and
        // neither earned one: a tab claims you will be there often, and the
        // account page is a twice-a-month visit while the codes page is a
        // ten-second lookup you leave immediately. They are REFERENCE surfaces
        // reached from the screen the app opens on, so they get the two corners
        // of its bar instead of permanent width in the tab bar — and both are
        // pushes onto this tab's own stack, so the back chevron returns you to
        // exactly the dashboard you left.
        //
        // The sides are the platform's habit, not a coin flip: a person glyph
        // leading is where every phone keeps "you", and the trailing corner is
        // the thumb's corner, which is the right one for the thing you reach for
        // while a login form is waiting on you.
        .toolbar {
            ToolbarItem(placement: .topBarLeading) {
                NavigationLink {
                    AccountPage()
                } label: {
                    Image(systemName: "person.crop.circle")
                }
                .accessibilityLabel("Account")
            }
            ToolbarItem(placement: .topBarTrailing) {
                NavigationLink {
                    MobileAuthView()
                } label: {
                    // A DOT WHEN SOMETHING JUST LANDED. The codes were a card on
                    // Quick Look, visible without asking; behind a glyph they are
                    // invisible, and a code that arrived while you were reading
                    // is the one moment this screen has to speak up. Same live
                    // window the page itself uses, so the dot and the row's
                    // "live" tag can never disagree.
                    //
                    // The padding RESERVES the dot's corner rather than offsetting
                    // it outside the glyph: a toolbar item is laid out inside its
                    // own glass capsule, and anything hung past the label's bounds
                    // is at the mercy of that capsule's clip.
                    Image(systemName: "key.fill")
                        .padding(.top, 3)
                        .padding(.trailing, 3)
                        .overlay(alignment: .topTrailing) {
                            if liveCode {
                                Circle()
                                    .fill(Palette.positive)
                                    .frame(width: 6, height: 6)
                            }
                        }
                }
                .accessibilityLabel("Login codes")
                .accessibilityValue(liveCodeValue)
            }
        }
        .refreshable {
            _ = await SitrepPoller.shared.pull()
            await store.refreshZones(force: true)
        }
        // The zones each ask for this too; the store joins the in-flight pass, so
        // asking here as well is what makes a cold launch paint from one round of
        // requests rather than from whichever zone happened to mount first.
        .task { await store.refreshZones() }
        .onAppear {
            ThreadPrefetch.shared.warm(
                store.sitrep.standing.map(\.thread_id), immediate: Self.eyesVisible,
                spacing: .milliseconds(150))
        }
        .onChange(of: visible.count) { _, count in
            if count == 0 { expanded = false }
        }
    }

    // MARK: - hero

    /// THE ONE SERIF LINE ON THIS SCREEN. Same sentence the Mac opens with, at a
    /// phone's measure — the brand's voice, said once, then out of the way.
    private var hero: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(Self.greeting() + (prefs.userName.isEmpty ? "" : ", \(prefs.userName)"))
                .font(Typo.micro)
                .foregroundStyle(Palette.accent)
                .textCase(.uppercase)
                .tracking(0.6)
            Text(headline)
                .font(Typo.hero(30))
                .foregroundStyle(Palette.ink)
                .lineSpacing(-1)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.top, 4)
    }

    private static let smallWords = [
        "Zero", "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Nine",
    ]

    /// Spell small counts — a numeral in a display serif headline reads like a
    /// dashboard metric, which is the opposite of the intent.
    private static func spell(_ n: Int) -> String {
        (0..<smallWords.count).contains(n) ? smallWords[n] : String(n)
    }

    private static func greeting(now: Date = Date()) -> String {
        let h = Calendar.current.component(.hour, from: now)
        if h < 12 { return "Good morning" }
        if h < 18 { return "Good afternoon" }
        return "Good evening"
    }

    private var headline: String {
        let today = SitrepView.needTodayCount(store.sitrep.standing)
        let total = store.sitrep.standing.count
        if today > 0 {
            return "\(Self.spell(today)) item\(today == 1 ? "" : "s") "
                + "need\(today == 1 ? "s" : "") you today."
        }
        if total > 0 {
            return "\(Self.spell(total)) item\(total == 1 ? "" : "s") on your plate."
        }
        return "You're all clear."
    }

    // MARK: - for your eyes

    /// The standing band, ranked. Tapping a row opens the reader with the WHOLE
    /// ranked list as its queue, which is what the Mac's list rows hand over —
    /// the dashboard's own rows pass no queue there because h/l is a keyboard
    /// walk, and a thumb has no such walk to protect.
    ///
    /// The rows swipe. They cannot use SwiftUI's `swipeActions` the way the mail
    /// tab does — a `List` inside this ScrollView would be two scrollers fighting
    /// — so `SwipeRow` draws the same rails by hand, off the same verb arrays, and
    /// the long press carries the rest of the keymap on both.
    private func forYourEyes(visible: [AttentionUpdate], overflow: Int, queue: [AttentionUpdate])
        -> some View
    {
        ZoneCard(symbol: "eye", title: "For your eyes", count: store.sitrep.standing.count) {
            VStack(spacing: 2) {
                ForEach(Array(visible.enumerated()), id: \.element.id) { _, u in
                    let verbs = UpdateVerbs(update: u, queue: queue)
                    SwipeRow(leading: verbs.leadingVerbs, trailing: verbs.trailingVerbs) {
                        UpdateRow(
                            update: u,
                            selected: false,
                            onHover: {},
                            onOpen: verbs.open)
                    }
                    .updateContextMenu(verbs)
                }
                if overflow > 0 {
                    Button {
                        withAnimation(Motion.disclose) { expanded.toggle() }
                    } label: {
                        Text(expanded ? "show less" : "\(overflow) more")
                            .font(Typo.micro)
                            .foregroundStyle(Palette.inkFaint)
                            .padding(.horizontal, 11)
                            .padding(.vertical, 5)
                    }
                    .buttonStyle(.glass)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.top, 6)
                }
            }
        }
    }

    // MARK: - footnote

    /// The page's last line: how fresh the board is, and what went wrong if
    /// anything did. The Mac says this in its masthead; a phone has no masthead
    /// to say it in, and "when did this last update" is the one question a
    /// glanceable surface owes an answer to.
    @ViewBuilder
    private var footnote: some View {
        if let refreshError = store.refreshError {
            Label(refreshError.message, systemImage: "exclamationmark.triangle.fill")
                .font(Typo.micro)
                .foregroundStyle(Palette.warn)
                .frame(maxWidth: .infinity, alignment: .leading)
        } else {
            Text(syncedLabel)
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    /// "synced 4m ago" / "synced just now" — never the nonsense "synced now ago".
    private var syncedLabel: String {
        guard let last = store.lastRefresh else { return "waiting for the first sync…" }
        let age = Fmt.relAge(last)
        return (age.isEmpty || age == "now") ? "synced just now" : "synced \(age) ago"
    }
}
