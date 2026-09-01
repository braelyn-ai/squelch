// THE QUICK LOOK TAB — the desktop sitrep's pinned right rail, given a phone
// destination of its own. Calendar / Shipments / Banking / Receipts are the SAME
// components the Mac's rail is built from (Views/SitrepZones.swift), and their
// own header says why they are a place rather than a feed: these are RECORDS,
// not actions. They are auto-resolved out of the attention bands at ingest, so
// this screen is their only surface, and each one renders empty state and all —
// a column that comes and goes as mail arrives is one you stop trusting.
//
// Everything here is something the app pulled out of your mail and is holding
// for you to look up, never something to answer. That is the tab's whole thesis,
// and it is why the name is Quick Look rather than Records — you come here to
// READ ONE FACT (the flight time, the tracking number) and leave.
//
// THE LOGIN CODES ARE BACK ON THIS TAB, but in its BAR and not in its column
// (Views/MobileAuthView.swift). They headed the column once and that was the
// wrong latency — a code you need in the ten seconds a login form is waiting on
// you does not belong under a scroll — and then they spent a while behind a key
// in the sitrep's bar, which cost the dashboard's trailing corner: the corner
// the composer wanted, for a door that is empty most days.
//
// SO THE MATERIAL SPLIT FROM THE URGENCY. What the key opens is a lookup — the
// sign-in alert from Tuesday, the reset you half remember asking for — and a
// lookup is exactly what this tab is for, one tap off the bar with a dot on it
// when something has landed. The one case that is a RACE does not come through
// here at all: a code-kind message inside the live window raises its own card at
// the top of the sitrep and reveals from there, so the most urgent row on the
// phone is still the cheapest one to reach.
//
// A Mac can pin all of this beside the work surface and a phone cannot, so what
// is one glance there is one tab here. Tapping a row opens the underlying email
// through `store.openThread`, exactly as it does on the Mac; the push itself is
// the shell's, bound to the same `store.threadId`.

import SwiftUI

struct MobileRecordsView: View {
    @Environment(AppStore.self) private var store

    /// Is there a sealed message young enough to still be worth racing to? The
    /// window and the test both belong to MobileAuthView, which is the surface
    /// that has to keep its word about them — this is only the badge asking.
    ///
    /// EVERY sealed kind, not just the code kinds the sitrep's card is gated on:
    /// the dot's job is "there is something new behind this key", and a sign-in
    /// alert is something new behind this key even though nobody is racing it.
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
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                masthead
                CalendarZone()
                ShipmentsZone()
                BankingZone()
                ReceiptsZone()
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 28)
        }
        .background(Palette.canvas)
        .navigationTitle("Quick Look")
        .navigationBarTitleDisplayMode(.inline)
        // THE KEY. A bare glyph rather than iOS 26's default glass capsule, for
        // the reason the sitrep's own bar buttons are bare: a capsule over a
        // column that is already four glass cards reads as chrome competing with
        // content. The accent stays, which is the platform's signal that a glyph
        // is a door.
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
                NavigationLink {
                    MobileAuthView()
                } label: {
                    // A DOT WHEN SOMETHING JUST LANDED. Behind a glyph the codes
                    // are invisible, and something arriving while you were
                    // elsewhere is the one moment this bar has to speak up. Same
                    // live window the page itself uses, so the dot and the row's
                    // "live" tag can never disagree.
                    //
                    // The padding RESERVES the dot's corner rather than hanging
                    // it outside the glyph: the item's own bounds are still what
                    // the toolbar lays out and clips against, capsule or no
                    // capsule, and a dot pushed past them is at that clip's mercy.
                    Image(systemName: "key.fill")
                        .foregroundStyle(Palette.accent)
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
            .sharedBackgroundVisibility(.hidden)
        }
        // Both halves, because a record can arrive either way: the zones come
        // from the zones fetch, and the resolution that MOVED a mail into one of
        // them rides the sitrep pull. Pulling only the zones would leave the
        // bands claiming an item this screen has already taken over.
        .refreshable {
            _ = await SitrepPoller.shared.pull()
            await store.refreshZones(force: true)
        }
        // Each zone asks for this too and the store joins the in-flight pass, so
        // asking here as well is what makes a cold open paint from one round of
        // requests rather than from whichever zone happened to mount first.
        .task { await store.refreshZones() }
    }

    /// THE ONE SERIF LINE ON THIS SCREEN, and the only thing on it that is not a
    /// zone: what these cards have in common, said once, so the tab reads as a
    /// place rather than as four unrelated widgets.
    private var masthead: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text("quick look")
                .font(Typo.micro)
                .foregroundStyle(Palette.accent)
                .textCase(.uppercase)
                .tracking(0.6)
            Text("Pulled out of your mail, kept here.")
                .font(Typo.hero(26))
                .foregroundStyle(Palette.ink)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.top, 4)
    }
}
