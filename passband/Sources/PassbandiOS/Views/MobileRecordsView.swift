// THE RECORDS TAB — the desktop sitrep's pinned right rail, given a phone
// destination of its own. Calendar / Shipments / Banking / Receipts are the
// SAME components the Mac's rail is built from (Views/SitrepZones.swift), and
// their own header says why they are a place rather than a feed: these are
// RECORDS, not actions. They are auto-resolved out of the attention bands at
// ingest, so this screen is their only surface, and each one renders empty
// state and all — a column that comes and goes as mail arrives is one you stop
// trusting.
//
// A Mac can pin them beside the work surface and a phone cannot, so what is one
// glance there is one tab here. Tapping a row opens the underlying email through
// `store.openThread`, exactly as it does on the Mac; the push itself is the
// shell's, bound to the same `store.threadId`.

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
        .navigationTitle("Records")
        .navigationBarTitleDisplayMode(.inline)
        .refreshable { await store.refreshZones(force: true) }
        // Each zone asks for this too and the store joins the in-flight pass, so
        // asking here as well is what makes a cold open paint from one round of
        // requests rather than from whichever zone happened to mount first.
        .task { await store.refreshZones() }
    }

    /// THE ONE SERIF LINE ON THIS SCREEN, and the only thing on it that is not a
    /// zone: what these four cards have in common, said once, so the tab reads as
    /// a place rather than as four unrelated widgets.
    private var masthead: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text("records")
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
