// THE QUICK LOOK TAB — the desktop sitrep's pinned right rail, given a phone
// destination of its own, plus the login codes. Calendar / Shipments / Banking
// / Receipts are the SAME components the Mac's rail is built from
// (Views/SitrepZones.swift), and their own header says why they are a place
// rather than a feed: these are RECORDS, not actions. They are auto-resolved
// out of the attention bands at ingest, so this screen is their only surface,
// and each one renders empty state and all — a column that comes and goes as
// mail arrives is one you stop trusting.
//
// The auth zone heads the tab because it is the same kind of thing: something
// the app pulled out of your mail and is holding for you to look up, never
// something to answer. That is the tab's whole thesis, and it is why the name
// is Quick Look rather than Records — you come here to READ ONE FACT (the code,
// the flight time, the tracking number) and leave.
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
                // FIRST, above everything. A login code is the most
                // time-critical thing in an inbox — you are standing at a login
                // form holding the phone that has the code on it — so it gets
                // the one position that never costs a scroll.
                MobileAuthZone()
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
        .refreshable { await store.refreshZones(force: true) }
        // Each zone asks for this too and the store joins the in-flight pass, so
        // asking here as well is what makes a cold open paint from one round of
        // requests rather than from whichever zone happened to mount first.
        .task { await store.refreshZones() }
    }

    /// THE ONE SERIF LINE ON THIS SCREEN, and the only thing on it that is not a
    /// zone: what these cards have in common, said once, so the tab reads as a
    /// place rather than as five unrelated widgets. Still true with the codes
    /// here — a login code is pulled out of your mail and kept exactly the way a
    /// tracking number is.
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
