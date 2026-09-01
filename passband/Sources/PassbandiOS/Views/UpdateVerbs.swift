// THE ROW VERBS, ONCE. Every touch gesture in this app — a swipe rail, a
// long-press menu, a deck card flung sideways — ends up in exactly one of these
// methods, and each method is the body of the desktop key binding that shares
// its name. Nothing here decides anything: it looks up what `e` does and does
// that.
//
// Read them against Views/EmailsView.swift's `bindings` (e/d/r/v/f/Enter) and
// Views/ActionLayer.swift's list bindings (t). The one place a verb is more than
// a single call — done, which resolves AND drops the row from the cached mail
// pages so the next poll cannot hand it back — is copied from `resolveSelected`
// rather than re-derived, and says so.
//
// A phone has no cursor, so there is no `actionable` guard to port: the row a
// gesture starts on IS the selection, and it is passed in.

import SwiftUI

/// The verb set for one row, bound to the queue that row's list would hand the
/// reader. Value type on purpose — a row builds one inline per gesture.
@MainActor
struct UpdateVerbs {
    let update: AttentionUpdate
    /// The list's own order, so "done + next" still walks it once the reader is
    /// open. The dashboard zones pass their ranked list; a zone with no walk of
    /// its own passes nothing, exactly as the Mac's zone rows do.
    var queue: [AttentionUpdate] = []

    private var store: AppStore { AppStore.shared }

    /// `Enter` / a row tap.
    func open() {
        store.openThread(update.thread_id, queue: queue)
    }

    /// `e` / `d`. Both halves of EmailsView.resolveSelected: the action, then the
    /// cached-page eviction that keeps the next poll from re-serving the row.
    func done() {
        Task { await Actions.done(update) }
        store.removeFromMail(update.id)
    }

    /// `e` in process mode — a DIFFERENT verb from done (status stays open, the
    /// INBOX label comes off), with its own inverse in the undo queue.
    func archive() {
        Task { await Actions.archive(update) }
    }

    /// `r`. Opens the reader with the reply state set; the composer that lands on
    /// the phone later is the surface, this is the state.
    func reply() {
        Actions.reply(update, queue: queue)
    }

    /// `i`, and only on the spam page. Rescues the message out of the provider's
    /// spam folder; see `Actions.notSpam` for why it has no undo.
    func notSpam() {
        Task { await Actions.notSpam(update) }
    }

    /// `t` (registered in ActionLayer for the list context).
    func tune() {
        Actions.tune(sender: update.sender)
    }

    /// `v`.
    func fixTriage() {
        store.openTriageFix(
            TriageFixTarget(
                messageId: update.id, sender: update.sender, subject: update.one_line,
                tier: .some(update.tier.rawValue)))
    }

    /// `f`. `sender` IS the address on this wire type — see the note on the
    /// desktop binding.
    func searchSender() {
        store.openSearch(seed: "from:\(update.sender)")
    }
}

// MARK: - the gesture vocabulary

/// One swipe rail button. The tint is the verb's semantic colour, shared by the
/// native `swipeActions` on the mail list and the hand-rolled rail on the
/// dashboard so a thumb learns one language for both.
struct SwipeVerb: Identifiable {
    let id = UUID()
    var title: String
    var symbol: String
    var tint: Color
    /// Destructive verbs are the full-swipe default on their edge.
    var destructive = false
    var run: () -> Void
}

extension UpdateVerbs {
    /// THE SPAM PAGE SWAPS BOTH RAILS, because three of the four ordinary verbs
    /// are meaningless on a row nothing triaged: tune argues with a verdict that
    /// was never reached, archive removes an INBOX label the message does not
    /// carry, and reply answers a machine. What is left is the one thing anybody
    /// opens this folder to do.
    ///
    /// It is a LEADING verb, deliberately not a full-swipe trailing one. Rescue
    /// is the verb with no undo behind it (see `Actions.notSpam`), and the
    /// trailing edge is where a thumb flicks without looking.
    var isSpamPage: Bool { AppStore.shared.mailMode == .spam }

    /// Trailing edge (drag LEFT): the resolve. Done leads because it is the one
    /// verb a triage run repeats, and it is what a full swipe commits.
    var trailingVerbs: [SwipeVerb] {
        if isSpamPage {
            return [
                SwipeVerb(
                    title: "Done", symbol: "checkmark", tint: Palette.positive, destructive: true,
                    run: done)
            ]
        }
        return [
            SwipeVerb(
                title: "Done", symbol: "checkmark", tint: Palette.positive, destructive: true,
                run: done),
            SwipeVerb(title: "Tune", symbol: "slider.horizontal.3", tint: Palette.accent, run: tune),
        ]
    }

    /// Leading edge (drag RIGHT): the two verbs that keep the mail — answering it
    /// or filing it. On the spam page, the one verb that keeps it for real.
    var leadingVerbs: [SwipeVerb] {
        if isSpamPage {
            return [
                SwipeVerb(
                    title: "Not spam", symbol: "tray.and.arrow.down", tint: Palette.positive,
                    run: notSpam)
            ]
        }
        return [
            SwipeVerb(
                title: "Reply", symbol: "arrowshape.turn.up.left", tint: Palette.accent, run: reply),
            SwipeVerb(title: "Archive", symbol: "archivebox", tint: Palette.warn, run: archive),
        ]
    }
}

// MARK: - long press

/// The remaining keymap, as a long-press menu: everything the two rails have no
/// room for. Same order the desktop help sheet lists them in.
struct UpdateContextMenu: ViewModifier {
    let verbs: UpdateVerbs

    func body(content: Content) -> some View {
        content.contextMenu {
            Button { verbs.open() } label: { Label("Open", systemImage: "envelope.open") }
            // The spam page's menu is the short one, for the reason its rails
            // are short: the verdict verbs have no verdict to act on.
            if verbs.isSpamPage {
                Button { verbs.notSpam() } label: {
                    Label("Not spam", systemImage: "tray.and.arrow.down")
                }
            } else {
                Button { verbs.reply() } label: {
                    Label("Reply", systemImage: "arrowshape.turn.up.left")
                }
                Divider()
                Button { verbs.tune() } label: {
                    Label("Tune this sender", systemImage: "slider.horizontal.3")
                }
                Button { verbs.fixTriage() } label: {
                    Label("Fix triage", systemImage: "wand.and.sparkles")
                }
                Button { verbs.searchSender() } label: {
                    Label("Search this sender", systemImage: "magnifyingglass")
                }
                Divider()
                Button { verbs.archive() } label: { Label("Archive", systemImage: "archivebox") }
            }
            Divider()
            // Destructive ROLE, not a destructive act: done is undoable for five
            // seconds like every other resolve. The role is what puts it last and
            // in red, which is where a thumb expects the one verb that empties
            // the row.
            Button(role: .destructive) { verbs.done() } label: {
                Label("Done", systemImage: "checkmark")
            }
        }
    }
}

extension View {
    func updateContextMenu(_ verbs: UpdateVerbs) -> some View {
        modifier(UpdateContextMenu(verbs: verbs))
    }
}

// MARK: - the mail list's rails

extension View {
    /// A `List` row wearing the app's skin rather than the system's: no
    /// separator, no grey selection slab, no inset chrome — the row's own glass
    /// and padding are the whole visual.
    func plainRow() -> some View {
        listRowBackground(Color.clear)
            .listRowSeparator(.hidden)
            .listRowInsets(EdgeInsets(top: 1, leading: 14, bottom: 1, trailing: 14))
    }

    /// The native rails, built from the SAME verb arrays the hand-rolled
    /// dashboard rail draws, so the two surfaces cannot drift on which verb sits
    /// on which edge or what colour it is.
    ///
    /// Full swipe on the trailing edge only. Leading is reply and archive: a
    /// stray flick that fires a reply state, or files mail on the way past, is a
    /// worse mistake than one that resolves it — done is the edge with a
    /// five-second undo behind it.
    func swipeVerbs(_ verbs: UpdateVerbs) -> some View {
        swipeActions(edge: .trailing, allowsFullSwipe: true) {
            ForEach(verbs.trailingVerbs) { verb in
                Button {
                    Haptics.commit()
                    verb.run()
                } label: {
                    Label(verb.title, systemImage: verb.symbol)
                }
                .tint(verb.tint)
            }
        }
        .swipeActions(edge: .leading, allowsFullSwipe: false) {
            ForEach(verbs.leadingVerbs) { verb in
                Button {
                    Haptics.commit()
                    verb.run()
                } label: {
                    Label(verb.title, systemImage: verb.symbol)
                }
                .tint(verb.tint)
            }
        }
    }
}
