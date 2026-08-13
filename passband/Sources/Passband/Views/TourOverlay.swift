// The first-run tour's one view: modals for the three talking steps, coach
// marks over the real dashboard for the two pointing steps, and a practice card
// for the two doing steps. Mounted from ActionLayer, so it sits OUTSIDE the
// blur layer and above the board it is describing.
//
// The tour pushes the "modal" KeyContext rather than a context of its own. That
// deliberately shadows the sitrep's keymap while it is up (nothing the board
// binds should fire under a lesson), and any overlay opened FROM the tour (the
// rule editor, the `?` sheet) pushes its own modal context above this one, so
// its keys and its Escape win until it closes.
//
// THE TOUR IS MODAL FOR ITS DURATION. `global` normally keeps composing under a
// modal, so the view nav it serves — 1..5, ⌘[ / ⌘] — is re-bound dead here, and
// the coach marks block clicks on the regions they ring. Leaving the board
// mid-lesson unmounts the sitrep whose measured rects the rings are drawn from,
// and hands Escape to whatever surface you landed on. Escape is the way out.

#if os(macOS)
    import AppKit
#endif
import SwiftUI

/// The coordinate space every measured region and the overlay itself report
/// into. Named on MainShell's outer ZStack, which is the common ancestor of the
/// sitrep and this overlay.
let tourSpace = "passband-tour"

extension View {
    /// Tag a dashboard region so a coach mark can ring it. Safe to leave on a
    /// zone permanently: outside a running tour the measurement lands in a
    /// non-observed shadow copy and invalidates nothing (see
    /// `TourController.report`).
    func tourTarget(_ id: TourTarget) -> some View {
        modifier(TourTargetModifier(id: id))
    }
}

private struct TourTargetModifier: ViewModifier {
    @Environment(AppStore.self) private var store
    let id: TourTarget

    func body(content: Content) -> some View {
        content
            .onGeometryChange(for: CGRect.self) {
                $0.frame(in: .named(tourSpace))
            } action: { store.tour.report(id, $0) }
    }
}

// MARK: - the overlay

struct TourOverlay: View {
    @Environment(AppStore.self) private var store

    /// This overlay's own frame in the tour space. Region rects arrive in that
    /// space, so subtracting this origin is what lets a ring be placed with a
    /// plain offset instead of a second GeometryReader.
    @State private var frame: CGRect = .zero
    /// Measured callout height, so the clamp against the window bottom knows
    /// how tall the thing it is clamping actually is.
    @State private var calloutHeight: CGFloat = 150

    private var tour: TourController { store.tour }

    /// Width of a coach-mark callout. Narrow on purpose: it sits BESIDE the
    /// region it is describing, and the region needs the room.
    private static let calloutWidth: CGFloat = 320

    var body: some View {
        ZStack(alignment: .topLeading) {
            // The doing steps dim the board without dismissing on click: the
            // lesson is two keystrokes long and a stray click must not end it.
            if isDemoStep && store.ruleEditor == nil {
                Rectangle()
                    .fill(.black.opacity(0.12))
                    .ignoresSafeArea()
                    .contentShape(Rectangle())
            }
            // The pointing steps are modal too, INVISIBLY: the ring has to keep
            // showing the real board, but a rail row clicked out from under a
            // coach mark would open a thread over the lesson. Same swallowing
            // click target as above, with nothing to see. The callout is stacked
            // after it, so it stays clickable.
            if isCoachStep {
                Rectangle()
                    .fill(.black.opacity(0.001))
                    .ignoresSafeArea()
                    .contentShape(Rectangle())
            }
            // ONE ring for both pointing steps, hoisted out of the step switch:
            // kept as a single view identity it TRAVELS from the records rail to
            // the newsletters zone instead of cross-fading between two rings.
            if let rect = ringRect {
                RoundedRectangle(cornerRadius: 18, style: .continuous)
                    .strokeBorder(Palette.accent, lineWidth: 2)
                    .frame(width: rect.width, height: rect.height)
                    .shadow(color: Palette.accent.opacity(0.45), radius: 18)
                    .offset(x: rect.minX, y: rect.minY)
                    .allowsHitTesting(false)
                    .transition(.opacity)
            }
            stepContent
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .onGeometryChange(for: CGRect.self) {
            $0.frame(in: .named(tourSpace))
        } action: { frame = $0 }
        .animation(.smooth(duration: 0.22), value: ringRect)
        .keyContext(.modal)
        .keyBindings(.modal, bindings)
    }

    // MARK: - steps

    @ViewBuilder
    private var stepContent: some View {
        switch tour.step {
        case .welcome:
            // The welcome is the pitch, so it gets hero treatment the other
            // talking steps do not: wider card, the app icon, numbers pulled
            // out of the prose into a stat row.
            modal(width: 640) { welcomeCard }
        case .records:
            coach(
                .records, title: "Records keep themselves",
                body:
                    "The everyday stuff keeps itself. Calendar, shipments, banking, and receipts live here so they never crowd your inbox.",
                locator: "They sit in the records rail, down the right of the sitrep.")
        case .newsletters:
            coach(
                .newsletters, title: "Newsletters, to one side",
                body: "Newsletters you might actually want, held to the side.",
                locator: "The newsletters zone sits under For your eyes.")
        case .philosophy:
            modal { philosophyCard }
        case .demoRule, .demoDone:
            // The real editor owns the screen while it is up: two cards
            // describing the same rule is one too many.
            if store.ruleEditor == nil {
                demoCard
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .center)
            }
        case .wrap:
            modal { wrapCard }
        }
    }

    /// The blurring steps. Backdrop click skips, exactly like every other modal
    /// in the app closes on its scrim.
    private func modal<Content: View>(
        width: CGFloat = 520, @ViewBuilder _ content: () -> Content
    ) -> some View {
        OverlayScrim(onDismiss: { tour.skip() }) {
            ModalCard(width: width) { content() }
        }
    }

    // MARK: - welcome

    /// The app's own icon, spelled per platform: AppKit hands it over directly;
    /// UIKit only offers the asset-catalog lookup, whose miss falls back to a
    /// symbol rather than crashing the welcome card.
    private var appIcon: Image {
        #if os(macOS)
            Image(nsImage: NSApplication.shared.applicationIconImage)
        #else
            if let icon = UIImage(named: "AppIcon") {
                Image(platformImage: icon)
            } else {
                Image(systemName: "envelope.circle.fill")
            }
        #endif
    }

    private var welcomeCard: some View {
        VStack(alignment: .leading, spacing: 16) {
            appIcon
                .resizable()
                .frame(width: 64, height: 64)
                .shadow(color: .black.opacity(0.25), radius: 10, y: 5)

            Text("Passband understands how you use email.")
                .font(Typo.serif(28, weight: .medium))
                .foregroundStyle(Palette.ink)
                .fixedSize(horizontal: false, vertical: true)

            if let numbers = welcomeNumbers {
                HStack(alignment: .top, spacing: 36) {
                    statTile(numbers.first.value, numbers.first.label, Palette.warn)
                    statTile(numbers.noise, "noise", Palette.danger)
                    statTile(numbers.standing, "need you", Palette.positive)
                }
                .padding(.vertical, 4)

                Text(numbers.line)
                    .font(.system(size: 13))
                    .foregroundStyle(Palette.inkDim)
                    .fixedSize(horizontal: false, vertical: true)
            } else {
                Text("It is still working through your mail. The board fills in as it goes.")
                    .font(.system(size: 13))
                    .foregroundStyle(Palette.inkDim)
                    .fixedSize(horizontal: false, vertical: true)
            }

            footer(key: "enter", "look around")
        }
        .padding(14)
    }

    /// One big number per column: the mailbox's own unread count when the
    /// daemon has fetched it, triage's all-time total otherwise, then noise and
    /// the standing count. nil while the first ingest is still running, which
    /// is the one moment there are no honest numbers to show.
    private var welcomeNumbers:
        (first: (value: Int, label: String), noise: Int, standing: Int, line: String)?
    {
        guard let stats = store.sitrep.stats, stats.total > 0 else { return nil }
        let noise = stats.tier_counts["noise"] ?? 0
        let standing = store.sitrep.standing.count
        let today = SitrepView.needTodayCount(store.sitrep.standing)
        let first = stats.inbox_unread.map { ($0.messages, "unread") } ?? (stats.total, "triaged")
        let line =
            today > 0
            ? "\(today) of those need you today. The rest is filed and waiting, not gone."
            : "The rest is filed and waiting, not gone."
        return (first, noise, standing, line)
    }

    private func statTile(_ value: Int, _ label: String, _ color: Color) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(value.formatted())
                .font(Typo.num(38, weight: .semibold))
                .foregroundStyle(color)
            Text(label)
                .font(Typo.sectionLabel)
                .textCase(.uppercase)
                .foregroundStyle(Palette.inkFaint)
        }
    }

    // MARK: - philosophy / wrap

    private var philosophyCard: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("You write the rules, in a sentence.")
                .font(Typo.serif(22, weight: .medium))
                .foregroundStyle(Palette.ink)
                .fixedSize(horizontal: false, vertical: true)

            Text(
                "Passband runs on rules you write in plain language. Tell it what you want and don't want, in a sentence. The more you tell it, the better triage gets."
            )
            .font(.system(size: 13))
            .foregroundStyle(Palette.inkDim)
            .fixedSize(horizontal: false, vertical: true)

            HStack(spacing: 14) {
                KeyHint(["j", "k"], "move")
                KeyHint("enter", "open")
                KeyHint("t", "teach it about a sender")
            }
            .padding(.top, 2)

            footer(key: "enter", "keep going")
        }
    }

    private var wrapCard: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("That is the lay of the land.")
                .font(Typo.serif(22, weight: .medium))
                .foregroundStyle(Palette.ink)
                .fixedSize(horizontal: false, vertical: true)

            HStack(spacing: 14) {
                KeyHint("⌘K", "ask your inbox anything")
                KeyHint("?", "every key")
                KeyHint("p", "walk new mail")
            }

            Text("Enjoy the quiet.")
                .font(.system(size: 13))
                .foregroundStyle(Palette.inkDim)

            footer(key: "enter", "start using it")
        }
    }

    // MARK: - coach marks

    /// Which region the ring is on, in overlay-local coordinates. nil on every
    /// step that is not a coach mark, and on a step whose region never
    /// reported (or scrolled out of the window) — see `usable`.
    ///
    /// The view check is belt and braces over the tour's own modality: the rects
    /// belong to a mounted sitrep, and a board that is no longer on screen left
    /// its last measurements behind. Anywhere else, the centered fallback.
    private var ringRect: CGRect? {
        guard store.activeView == .sitrep else { return nil }
        guard let id = ringTarget, let raw = tour.targets[id] else { return nil }
        let rect = raw.offsetBy(dx: -frame.minX, dy: -frame.minY)
        return usable(rect) ? rect : nil
    }

    private var ringTarget: TourTarget? {
        switch tour.step {
        case .records: .records
        case .newsletters: .newsletters
        default: nil
        }
    }

    /// A region worth ringing: real size, and enough of it inside the window
    /// that a ring would read as pointing at something.
    private func usable(_ rect: CGRect) -> Bool {
        guard frame.width > 0, frame.height > 0 else { return false }
        guard rect.width > 60, rect.height > 40 else { return false }
        return rect.maxY > 80 && rect.minY < frame.height - 60
            && rect.maxX > 0 && rect.minX < frame.width
    }

    /// A pointing step: ring plus a callout beside it. With no usable rect the
    /// callout centers itself and the copy says where the region lives, which
    /// is the honest fallback for a scrolled-away or never-measured zone.
    @ViewBuilder
    private func coach(
        _ id: TourTarget, title: String, body: String, locator: String
    ) -> some View {
        if let rect = ringRect, ringTarget == id {
            let origin = calloutOrigin(rect)
            calloutCard(title: title, body: body)
                .frame(width: Self.calloutWidth)
                .onGeometryChange(for: CGFloat.self) { $0.size.height } action: {
                    calloutHeight = $0
                }
                .offset(x: origin.x, y: origin.y)
        } else {
            calloutCard(title: title, body: body, locator: locator)
                .frame(width: 420)
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .center)
        }
    }

    private func calloutCard(title: String, body: String, locator: String? = nil) -> some View {
        VStack(alignment: .leading, spacing: 9) {
            Text(title)
                .font(.system(size: 14, weight: .semibold))
                .foregroundStyle(Palette.ink)
            Text(body)
                .font(Typo.rowSub)
                .foregroundStyle(Palette.inkDim)
                .fixedSize(horizontal: false, vertical: true)
            if let locator {
                Text(locator)
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                    .fixedSize(horizontal: false, vertical: true)
            }
            footer(key: "enter", "next")
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .passbandGlass(.pane, cornerRadius: 18, tint: Palette.glassTint)
        .shadow(color: .black.opacity(0.3), radius: 34, y: 14)
    }

    /// Beside the region if either side has room, under it (or over it) if
    /// neither does, and always clamped inside the window.
    private func calloutOrigin(_ rect: CGRect) -> CGPoint {
        let width = Self.calloutWidth
        let gap: CGFloat = 16
        let margin: CGFloat = 18
        var x: CGFloat
        var y = rect.minY

        if rect.minX - gap - width >= margin {
            x = rect.minX - gap - width
        } else if rect.maxX + gap + width <= frame.width - margin {
            x = rect.maxX + gap
        } else {
            x = rect.midX - width / 2
            y = rect.maxY + gap
            if y + calloutHeight > frame.height - margin {
                y = rect.minY - gap - calloutHeight
            }
        }
        x = min(max(x, margin), max(margin, frame.width - width - margin))
        y = min(max(y, margin), max(margin, frame.height - calloutHeight - margin))
        return CGPoint(x: x, y: y)
    }

    // MARK: - the demo

    private var isDemoStep: Bool { tour.step == .demoRule || tour.step == .demoDone }

    private var isCoachStep: Bool { tour.step == .records || tour.step == .newsletters }

    /// Whether the practice chip is still on the undo stack. It reaps after 5s,
    /// and copy pointing at a chip that is no longer there is a lie.
    private var demoUndoPending: Bool {
        store.undos.contains { $0.messageId == TourController.demoUpdate.id }
    }

    /// The practice card. The row is a real `UpdateRow` over the tour's own
    /// fake update, so the thing being taught looks exactly like the thing it
    /// is teaching about.
    private var demoCard: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 4) {
                Text("practice email")
                    .font(Typo.sectionLabel)
                    .foregroundStyle(Palette.inkFaint)
                    .textCase(.uppercase)
                Spacer()
                Text("nothing here touches your mail")
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
            }

            UpdateRow(
                update: TourController.demoUpdate, selected: tour.demoPhase != .done,
                onHover: {}, onOpen: {}
            )
            .opacity(tour.demoPhase == .done ? 0.4 : 1)
            .allowsHitTesting(false)

            if tour.demoPhase == .ruleSaved || tour.demoPhase == .done
                || tour.demoPhase == .undone
            {
                Text(
                    "That sentence is now a standing rule. Future terms of service updates from them get filed without asking you."
                )
                .font(Typo.rowSub)
                .foregroundStyle(Palette.positive)
                .fixedSize(horizontal: false, vertical: true)
            }

            Text(demoInstruction)
                .font(.system(size: 14))
                .foregroundStyle(Palette.ink)
                .fixedSize(horizontal: false, vertical: true)

            if tour.step == .demoRule {
                Text("Or press enter to move on.")
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
            }
            if tour.step == .demoDone, tour.demoPhase == .done, demoUndoPending {
                Text("The undo chip is bottom left. Clicking it does the same thing.")
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
            }

            footer(key: demoLead.key, demoLead.label)
        }
        .padding(20)
        .frame(width: 560)
        .passbandGlass(.pane, cornerRadius: 20, tint: Palette.glassTint)
        .shadow(color: .black.opacity(0.32), radius: 44, y: 18)
    }

    private var demoInstruction: String {
        switch tour.step {
        case .demoRule:
            return "Here is a practice email. Press t to teach Passband about this sender."
        default:
            switch tour.demoPhase {
            case .done: return "Everything is reversible for a few seconds. Press u."
            case .undone: return "Back where it was."
            default: return "Done with it? Press e."
            }
        }
    }

    private var demoLead: (key: String, label: String) {
        if tour.step == .demoRule { return ("t", "write the rule") }
        switch tour.demoPhase {
        case .done: return ("u", "undo it")
        case .undone: return ("enter", "next")
        default: return ("e", "mark it done")
        }
    }

    // MARK: - shared footer

    /// Every step's footer: the step's ONE next action, loud, then the place
    /// count and the escape route small and right-justified. The hierarchy is
    /// the lesson plan — what to press next must be the first thing the eye
    /// lands on, and leaving must always be possible but never the headline.
    private func footer(key: String, _ label: String) -> some View {
        HStack(spacing: 10) {
            HStack(spacing: 7) {
                Text(key)
                    .font(Typo.mono(12, weight: .semibold))
                    .foregroundStyle(.white)
                    .padding(.horizontal, 8)
                    .padding(.vertical, 4)
                    .background(
                        RoundedRectangle(cornerRadius: 6, style: .continuous)
                            .fill(Palette.accent)
                    )
                Text(label)
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(Palette.accent)
            }
            Spacer(minLength: 12)
            Text("\(tour.step.position) / \(TourStep.allCases.count)")
                .font(Typo.num(11))
                .foregroundStyle(Palette.inkFaintest)
            Button {
                tour.skip()
            } label: {
                HStack(spacing: 4) {
                    Kbd("esc")
                    Text("skip tour")
                }
            }
            .buttonStyle(.textAction)
            .font(Typo.micro)
        }
        .padding(.top, 6)
    }

    // MARK: - keymap

    private var bindings: [KeyBinding] {
        var bindings: [KeyBinding] = [
            KeyBinding("Escape", "skip tour") { tour.skip() }
        ]
        // Forward on every step, finishing on the last. It is also the escape
        // hatch out of the two demo steps: a lesson nobody can leave by walking
        // forward is a trap, and the editor's own Enter (save) outranks this one
        // while the editor is up.
        let forward = tour.step == .wrap ? "finish the tour" : "next"
        bindings.append(KeyBinding("Enter", forward) { tour.advance() })
        bindings.append(KeyBinding("ArrowRight", forward) { tour.advance() })
        if tour.step != .welcome {
            bindings.append(KeyBinding("ArrowLeft", "previous step") { tour.back() })
        }
        bindings.append(contentsOf: navHolds)
        switch tour.step {
        case .demoRule:
            bindings.append(
                KeyBinding("t", "teach passband about this sender") { openDemoRule() })
        case .demoDone:
            bindings.append(KeyBinding("e", "mark the practice email done") { markDemoDone() })
            bindings.append(KeyBinding("d", "mark the practice email done") { markDemoDone() })
            // Declining so `u` stays free before the demo has anything to undo;
            // the global undo binding is right behind it and equally inert.
            //
            // NEVER `store.fireUndo`: the practice action never left the
            // machine, so firing it would claim an undo_fired event for it and
            // force a sitrep pull whose failure raises the stale-data banner
            // over the lesson. The chip is dropped by hand and the revert is
            // the controller's. Clicking the real chip still goes the long way,
            // which is the point of teaching it.
            bindings.append(
                KeyBinding(declining: "u", "undo the practice action") {
                    guard tour.demoPhase == .done else { return false }
                    store.undos.removeAll { $0.messageId == TourController.demoUpdate.id }
                    tour.noteUndone()
                    return true
                })
        default:
            break
        }
        return bindings
    }

    /// View nav, held for the tour's duration. Registered in "modal", which
    /// dispatch consults ahead of the "global" set these shadow.
    ///
    /// 1..5 stay input-guarded, so a digit typed into the rule editor the tour
    /// opens is still a digit. The ⌘ chords are not typed characters, so they
    /// are swallowed with a field focused too — otherwise ⌘[ from inside that
    /// editor walks off the board.
    private var navHolds: [KeyBinding] {
        var holds = (1...MainView.mainViews.count).map { index in
            KeyBinding("\(index)", "held · the tour is running") {}
        }
        holds.append(
            KeyBinding("[", "held · the tour is running", meta: true, allowInInput: true) {})
        holds.append(
            KeyBinding("]", "held · the tour is running", meta: true, allowInInput: true) {})
        return holds
    }

    /// `t`: the REAL rule editor, prefilled, with an interceptor that keeps the
    /// save client-side. A genuine POST would create a rule the user never
    /// asked for, and would 403 outright on a read-only daemon.
    private func openDemoRule() {
        let tour = self.tour
        store.openRuleEditor(
            RuleEditorRequest(
                sender: TourController.demoUpdate.sender,
                want: "I don't need to see terms of service updates",
                intercept: { _ in },
                onSaved: { tour.noteRuleSaved() }))
    }

    /// `e`: the undo chip, exercised for real, with a client-only revert. Never
    /// `Actions.done` — that would fire a status write against message -1.
    private func markDemoDone() {
        guard tour.demoPhase != .done, tour.demoPhase != .undone else { return }
        let tour = self.tour
        tour.noteDone()
        store.pushUndo(
            kind: .done, messageId: TourController.demoUpdate.id,
            label: "done: Brightly · terms of service update"
        ) {
            await MainActor.run { tour.noteUndone() }
        }
    }
}
