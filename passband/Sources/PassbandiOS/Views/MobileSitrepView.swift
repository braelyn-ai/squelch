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
// WHAT IT GAINED IS TWO CORNERS. The navigation bar carries the account page
// (leading) and the composer (trailing), because this is the screen the app
// opens on and neither of those was worth a tab. They are in the CHROME and not
// in the column on purpose: the scroll is obligations, and a bar button is
// reachable without disturbing a single one of them.
//
// THE TRAILING CORNER USED TO BE THE KEY, and the swap is a trade of a door you
// open most days for one that is empty most days. Writing an email is the thing
// this app could not do from a phone at all (#166); the login codes are a lookup,
// and a lookup belongs on Quick Look with the other things the app pulled out of
// your mail and is holding (MobileRecordsView's bar carries the key now, dot and
// all).
//
// WITH ONE EXCEPTION, AND IT IS THE TOP OF THIS COLUMN. A code that just landed
// is not a lookup — you are standing at a login form holding the phone the code
// is on — so it does not wait behind a glyph on another tab. It raises a card
// ABOVE the hero, reveals from there, and disappears when it goes stale. That
// card is the only thing on this screen allowed to outrank the greeting, and it
// is allowed because it is the one thing here that expires.

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

    /// The codes young enough to still be worth racing to. The window, the
    /// code-kind test and the sort all belong to MobileAuthView, which is the
    /// surface that has to keep its word about them — this is only the card
    /// asking.
    private var freshCodes: [SealedMeta] {
        MobileAuthView.freshCodes(store.sitrep.sealed)
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
                // ABOVE THE HERO, and only ever for the minutes it is true.
                // Everything else in this column is work that will still be
                // there tomorrow; a login code is the one thing on the screen
                // with a clock running on it, and a card under the greeting is
                // a card you scroll to.
                let fresh = freshCodes
                if !fresh.isEmpty {
                    FreshCodeCard(codes: fresh)
                }
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
        // THE TWO DOORS OFF THE DASHBOARD: who you are, and the one verb this
        // app owes you that no amount of triage can stand in for. The account
        // page used to be a tab and did not earn one — a tab claims you will be
        // there often and it is a twice-a-month visit — so it gets a corner of
        // this bar instead of permanent width in the tab bar.
        //
        // The sides are the platform's habit, not a coin flip: a person glyph
        // leading is where every phone keeps "you", and the trailing corner is
        // the thumb's corner, which is the right one for the thing you actually
        // reach for.
        //
        // BARE GLYPHS, NOT BUTTONS. iOS 26 gives a toolbar item its own glass
        // capsule by default, and two of them over a dashboard that is already
        // a column of glass cards reads as chrome competing with content. The
        // background goes; the accent stays, which is the platform's own signal
        // that a glyph is a door and the reason a bare icon still looks tappable.
        .toolbar {
            ToolbarItem(placement: .topBarLeading) {
                NavigationLink {
                    AccountPage()
                } label: {
                    Image(systemName: "person.crop.circle")
                        .foregroundStyle(Palette.accent)
                }
                .accessibilityLabel("Account")
            }
            .sharedBackgroundVisibility(.hidden)
            ToolbarItem(placement: .topBarTrailing) {
                // THE COMPOSER, ON THE SCREEN THE APP OPENS ON. Until #166 the
                // phone could not write an email at all, and the fix is not a
                // fifth tab: composing is a verb, not a place, and a verb wants
                // the corner your thumb is already resting on.
                //
                // It opens the SAME `store.compose` the mail tab's button and
                // the Mac's `c` open — one composer, one draft, one autosave —
                // and the sheet it raises is mounted on the shell, so the
                // dashboard is what you come back to.
                Button {
                    store.openComposeNew()
                } label: {
                    Image(systemName: "square.and.pencil")
                        .foregroundStyle(Palette.accent)
                }
                .accessibilityLabel("New message")
            }
            .sharedBackgroundVisibility(.hidden)
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
                    // THE WHOLE ROW IS THE BUTTON, and only the pill is drawn.
                    // A thumb aims at a line of a list, not at a 60pt chip at
                    // the end of it, and every row above this one is already
                    // tappable edge to edge — a control that looks like it
                    // belongs to that stack and answers to a third of its width
                    // reads as a miss rather than as a smaller target.
                    //
                    // So the glass is painted by hand (`glassCapsule`) inside a
                    // `.plain` button and `contentShape` claims the full width:
                    // `.buttonStyle(.glass)` would put the hit area exactly where
                    // the pill is, which is the problem.
                    Button {
                        withAnimation(Motion.disclose) { expanded.toggle() }
                    } label: {
                        HStack(spacing: 0) {
                            Text(expanded ? "show less" : "\(overflow) more")
                                .font(Typo.micro)
                                .foregroundStyle(Palette.inkFaint)
                                .padding(.horizontal, 11)
                                .padding(.vertical, 5)
                                .glassCapsule()
                            Spacer(minLength: 0)
                        }
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
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

// MARK: - the code that just landed

/// THE ONE CARD ON THIS SCREEN WITH A CLOCK ON IT. Everything else in the sitrep
/// is work that will still be there tomorrow; this is a login code that arrived
/// in the last hour, drawn only while that is true and gone on its own after.
///
/// IT REVEALS FROM HERE. The point of raising it above the hero is to make the
/// distance from "the app is open" to "the digits are on screen" one tap, so
/// sending the user to the codes page first would have given the card its
/// urgency and then charged for it anyway. The tap runs
/// `MobileAuthView.revealCode` — the same audited call the page's own rows run —
/// and the digits land in `store.authQueue`, which the shell already watches:
/// AuthCodeModal comes up with its 30s self-destruct and its copy button, and
/// nothing here ever holds a code.
///
/// AND IT DRAWS `AuthRow`, the page's row, not a lookalike. A card that rendered
/// the same sealed message a little differently from the page behind it would be
/// two answers to "what just arrived".
///
/// TWO ROWS, THEN IT DEFERS. Codes arrive one at a time in practice; two is the
/// generous case (a retry, or two services at once) and anything past it is a
/// list, which is what the page is for. A card that could grow without bound is
/// a card that can push the whole dashboard off the screen.
private struct FreshCodeCard: View {
    @Environment(AppStore.self) private var store
    /// Newest first, already filtered to live code kinds by
    /// `MobileAuthView.freshCodes` — this view does not decide what is fresh.
    let codes: [SealedMeta]

    private static let visible = 2

    /// Which row is mid-reveal. Local, like the page's: a reveal in flight is
    /// this card's business and outlives nothing.
    @State private var busy: Int?

    var body: some View {
        ZoneCard(
            symbol: "key.fill",
            title: "Just arrived",
            count: codes.count > 1 ? codes.count : nil,
            subtitle: "sealed until you tap",
            tint: Palette.lock
        ) {
            VStack(spacing: 2) {
                ForEach(codes.prefix(Self.visible)) { meta in
                    AuthRow(
                        meta: meta,
                        live: true,
                        busy: busy == meta.id,
                        onReveal: { Task { await reveal(meta) } })
                }
                if codes.count > Self.visible {
                    NavigationLink {
                        MobileAuthView()
                    } label: {
                        Text("\(codes.count - Self.visible) more")
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

    /// No kind branch, unlike the page's: `freshCodes` already promised every
    /// row here has digits to present, so there is no RevealPanel case to host
    /// and this card never has to become a second reading surface.
    private func reveal(_ meta: SealedMeta) async {
        guard busy == nil else { return }
        busy = meta.id
        defer { busy = nil }
        await MobileAuthView.revealCode(meta, into: store)
    }
}
