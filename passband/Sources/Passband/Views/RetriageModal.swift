// THE ONE MODAL THAT DOES NOT CLOSE. A dev re-triage rewrites tiers, bands and
// every specialist row underneath whatever page you were reading, and it takes
// minutes — so rather than let the board lie for the duration, this covers it
// and reports the queues draining.
//
// It still has a way out. The re-triage is the DAEMON's work and runs whether
// anyone is watching, so a stuck poll (or a daemon too old to be polled) must
// hand the window back instead of holding it forever behind a counter that
// cannot move. Closing stops the watching, never the run.

import SwiftUI

struct RetriageModal: View {
    @Environment(AppStore.self) private var store
    let run: RetriageRun

    var body: some View {
        // NO DISMISS ON THE SCRIM — deliberately an empty closure rather than an
        // omitted one. The tap still lands here and dies here, which is what
        // stops a click from reaching the board behind it.
        OverlayScrim(onDismiss: {}) {
            ModalCard(width: 380, tint: Palette.glassTintStrong) {
                header
                counter
                bar
                footer
            }
        }
        .keyContext(.modal)
        // Esc is bound to NOTHING while the run is live: the whole point is that
        // the app is unavailable, and a modal that Esc closes is not blocking.
        // Once the wait can no longer end on its own, Esc is the way out.
        .keyBindings(.modal, run.canClose ? [
            KeyBinding("Escape", "close") { store.endRetriage() },
            KeyBinding("Enter", "close") { store.endRetriage() },
        ] : [])
    }

    private var header: some View {
        HStack(spacing: 9) {
            Image(systemName: "arrow.trianglehead.2.clockwise")
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(Palette.accent)
                .symbolEffect(.rotate, isActive: run.watching)
            Text(title)
                .font(.system(size: 14, weight: .semibold))
                .foregroundStyle(Palette.ink)
            Spacer(minLength: 0)
        }
    }

    private var title: String {
        if run.failure != nil { return "Re-triage: lost track" }
        if run.unsupported { return "Re-triage running" }
        if run.stalled { return "Re-triage: not moving" }
        return "Re-triage in progress"
    }

    /// The two numbers, as big as they are because they are the only reason the
    /// window is gone. Before the kick answers there is no denominator yet, and
    /// "0 of 0" is a worse thing to show for that second than a sentence.
    @ViewBuilder private var counter: some View {
        if run.total == 0 && !run.counted {
            HStack {
                Text("sizing the queue…")
                    .font(Typo.num(15))
                    .foregroundStyle(Palette.inkFaint)
                Spacer(minLength: 0)
            }
            .frame(height: 36, alignment: .bottom)
        } else {
            numbers
        }
    }

    private var numbers: some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            Text("\(run.done)")
                .font(Typo.num(30, weight: .semibold))
                .foregroundStyle(Palette.ink)
                .contentTransition(.numericText())
                .animation(.smooth(duration: 0.25), value: run.done)
            Text("of \(run.total)")
                .font(Typo.num(15))
                .foregroundStyle(Palette.inkFaint)
            Text(run.total == 1 ? "email" : "emails")
                .font(Typo.rowSub)
                .foregroundStyle(Palette.inkFaintest)
            Spacer(minLength: 0)
        }
    }

    private var bar: some View {
        GeometryReader { geo in
            ZStack(alignment: .leading) {
                Capsule().fill(Palette.hairline)
                Capsule()
                    .fill(run.finished ? Palette.positive : Palette.accent)
                    .frame(width: max(0, geo.size.width * run.fraction))
                    // The counter's own animation, so the number and the bar
                    // arrive together instead of the bar chasing it.
                    .animation(.smooth(duration: 0.25), value: run.fraction)
            }
        }
        .frame(height: 4)
    }

    @ViewBuilder private var footer: some View {
        if let failure = run.failure {
            note(failure, tone: Palette.danger)
            closeButton("Close")
        } else if run.unsupported {
            note(
                "this daemon can't report progress — update squelchd to watch it. "
                    + "The re-triage itself is running.", tone: Palette.warn)
            closeButton("Close")
        } else if run.stalled {
            // Still polling — the run may simply be behind a slow cycle — but
            // the door is open now.
            note(
                "the counter hasn't moved in a while. It may still be working; "
                    + "closing this stops the watching, not the run.", tone: Palette.warn)
            closeButton("Close anyway")
        } else if !run.counted {
            note("asking the daemon where it is…", tone: Palette.inkFaintest)
        } else {
            note(
                "the board is unavailable until this finishes, because every tier "
                    + "on it is about to change.", tone: Palette.inkFaintest)
        }
    }

    private func note(_ text: String, tone: Color) -> some View {
        Text(text)
            .font(Typo.micro)
            .foregroundStyle(tone)
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func closeButton(_ label: String) -> some View {
        HStack {
            Spacer(minLength: 0)
            Button(label) { store.endRetriage() }
                .buttonStyle(.glass)
        }
    }
}
