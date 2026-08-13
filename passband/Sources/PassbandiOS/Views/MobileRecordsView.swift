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
// THE LOGIN CODES USED TO HEAD THIS COLUMN and now live behind the key in the
// sitrep's navigation bar (Views/MobileAuthView.swift). Same kind of material,
// wrong latency: the other four are things you go looking for, and a code is
// something you need in the ten seconds a login form is waiting on you. Making
// that a tab hop plus a scroll was charging the most urgent row on the phone the
// highest price to reach.
//
// A Mac can pin all of this beside the work surface and a phone cannot, so what
// is one glance there is one tab here. Tapping a row opens the underlying email
// through `store.openThread`, exactly as it does on the Mac; the push itself is
// the shell's, bound to the same `store.threadId`.

import SwiftUI

struct MobileRecordsView: View {
    @Environment(AppStore.self) private var store

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
