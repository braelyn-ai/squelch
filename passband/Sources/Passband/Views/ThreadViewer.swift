// FULLSCREEN THREAD VIEWER. Fetches GET /client/thread/{id} and stacks every
// message IN ORDER — oldest at the top, newest at the bottom, the way the
// conversation happened — and OPENS ON THE NEWEST, parked at the top of the
// window with the history above it to scroll back into. j/k and the arrows move
// the selection, which always comes to rest at the top of the window (see
// `tailSpace` for the one trick that makes that possible for the last message).
//
// HTML renders in the hard-sandboxed EmailWebView, plain text in a selectable
// card. The outer column is the ONE scroll surface — message frames size to
// their content and never scroll internally. Esc, or a gutter click, closes back
// onto the surface below, and a minimap rail down the left edge says where in
// the conversation you are.
//
// It is also where you REPLY: `r` (or an `r` pressed on a list row, handed over
// as `pendingReplyMessageId`) pins InlineReply under the stack, still inside this
// surface.

import SwiftUI

struct ThreadViewer: View {
    let threadId: String

    @Environment(AppStore.self) private var store
    @Environment(Prefs.self) private var prefs

    /// Computed, not stored: a stored property here would drag the memberwise
    /// initializer down to `private` with it.
    private var styles: ThreadStyleLedger { .shared }

    @State private var thread: ClientThreadView?
    @State private var error: String?
    @State private var loading = true
    /// HOW THIS THREAD IS DRAWN, resolved once per thread rather than per row:
    /// this thread's own answer if it has ever been given one, else the global
    /// default. Held in state so the toggle can flip it under the reader.
    @State private var style: ThreadStyle = .classic
    /// The thread id `style` was resolved FOR. The toggle pins forever, so it
    /// must know the difference between an answer and a leftover: on a switch
    /// this view stays mounted and holds the LAST thread's style until the new
    /// one's `adopt` runs — a gap the load flag does not cover, because the
    /// prefetch-cache path never raises it.
    @State private var styledFor: String?
    /// Selected message: j/k move it, click selects, the rail highlights it.
    @State private var index = 0
    /// Existing unsubscribe record for THIS thread's sender; drives the header
    /// hint copy. nil = none.
    @State private var unsub: UnsubscribeRecord?
    /// nil = closed; .ask = "Unsubscribe from X?"; .noLink = the 422 fallback.
    @State private var confirmMode: ConfirmMode?
    @State private var confirmBusy = false
    @State private var retriaging = false
    @State private var debugInfo: TriageDebug?
    /// True for `Clip.flashWindow` after the subject is clicked-to-copy.
    @State private var subjectCopied = false
    /// messageId -> recorded opens of the user's own tracked sends. Only sent,
    /// tracked messages ever have an entry; see `refreshOpens`.
    @State private var opens: [Int: [MessageOpen]] = [:]
    /// WHERE THE MESSAGE CARDS ARE IN THE WINDOW, which is what the minimap
    /// draws. Kept in an object rather than in `@State` on purpose: it is
    /// rewritten on every scroll tick, and this view's body must not be
    /// invalidated at that rate — a re-render here walks every message card and
    /// every sandboxed web frame. Only the minimap reads it, so only the minimap
    /// redraws.
    @State private var map = ThreadMap()
    /// The scroll viewport's height. Changes when the WINDOW does, not when the
    /// mail moves, so it is cheap to keep here — and `tailSpace` needs it.
    @State private var viewportHeight: CGFloat = 0
    /// The newest message card's laid-out height, the other half of `tailSpace`.
    @State private var newestHeight: CGFloat = 0
    /// The rail's drawing for the thread that is loaded. Held rather than
    /// derived per render — see `marks(for:)`.
    @State private var marks: [ThreadMinimap.Mark] = []
    /// The width a message CARD lays out at, measured off the column itself.
    /// Style-independent on purpose — it changes only when the window does —
    /// which is what lets the height memory hold one width for both styles: the
    /// key carries the style (ThreadStyle.frameKey) and the measure below
    /// derives from the two together.
    @State private var columnWidth: CGFloat = 0
    /// Whether the opening landing has been taken for this thread. Until it has,
    /// the reader is wherever the initial anchor dropped it, which is not a
    /// position anybody chose.
    @State private var landed = false
    /// True while a hand is on the scroll — see `onScrollPhaseChange`. It is
    /// what keeps a late measurement from correcting a position the reader is
    /// in the middle of choosing.
    @State private var handScrolling = false
    /// The re-aiming loop, held so the next one can end it: two live settles
    /// issue competing `scrollTo`s at two targets, which is a column that
    /// judders rather than lands. See `settle`.
    @State private var settleTask: Task<Void, Never>?

    enum ConfirmMode: Equatable { case ask, noLink }

    /// What a measuring pass is FOR. Any of the three changing means the
    /// heights on file answer a question nobody is asking any more: another
    /// thread, another message in this one, another column width.
    private struct MeasurePass: Equatable {
        let thread: String
        let count: Int
        let width: CGFloat
        /// A bubble measures its document at the bubble's measure, so a flip is
        /// a whole new set of heights to take — under a whole new set of keys.
        let style: ThreadStyle
        /// Until the thread has chosen a style there is no measure to render
        /// at, and a pass taken at the wrong one files every height under a key
        /// nothing will read.
        let ready: Bool
    }

    /// Server order IS display order: chronological, oldest first. The reader
    /// opens parked on the last one.
    private var messages: [ClientMessage] { thread?.messages ?? [] }
    /// Display index of the newest message — the landing spot, and the floor
    /// `anchorId` measures "still at the bottom" against.
    private var newestIndex: Int { max(0, messages.count - 1) }
    /// The NEWEST message is what `u` acts on (the server derives the sender
    /// from it) and whose from_addr keys the record lookup.
    private var newest: ClientMessage? { messages.last }
    /// What `h` parks: the newest message the user did NOT send.
    ///
    /// Not the selected one, which in a replied thread is typically your own
    /// reply — and a reminder on your own sent mail is a reminder on mail no
    /// listing will ever show, so the daemon answers those with a 404. A thread
    /// that is nothing BUT your own mail (sent, no answer back yet) falls back
    /// to the selected message and lets the daemon have the last word. `nil`
    /// reads as inbound on purpose: an older daemon sends no flag at all, and a
    /// missing one has to leave the key working exactly as it did.
    private var remindable: ClientMessage? {
        messages.last { $0.is_sent != true } ?? messages[safe: index]
    }
    /// trim().lowercased() mirrors the server's canonical `sender`.
    private var newestSender: String? {
        newest.map { $0.from_addr.trimmingCharacters(in: .whitespaces).lowercased() }
    }
    private var senderName: String { newest.map { SenderCache.resolved($0.senderString).displayName } ?? "" }

    /// EVERY participant, in first-appearance order, deduped by canonical address,
    /// so the list starts with whoever started the thread.
    private var participants: [String] {
        var seen = Set<String>()
        var names: [String] = []
        for m in messages {
            let key = m.from_addr.trimmingCharacters(in: .whitespaces).lowercased()
            guard seen.insert(key).inserted else { continue }
            names.append(SenderCache.resolved(m.senderString).displayName)
        }
        return names
    }

    /// "Alice, Bob, Carol +3" — the header is a fixed strip above a scroll, so a
    /// long thread must collapse into a count rather than shove the mail down.
    private var participantLine: String {
        let names = participants
        guard names.count > 3 else { return names.joined(separator: ", ") }
        return names.prefix(3).joined(separator: ", ") + " +\(names.count - 3)"
    }

    /// Display index of the message needing attention — the LAST one, so a thread
    /// with two obligations points at the most recent. nil for a calm thread.
    private var attentionIndex: Int? {
        messages.lastIndex(where: \.needsAttention)
    }

    /// The jump chip's copy: the deadline chip text when a date exists
    /// ("12d PAST DUE" / "due Aug 15"), else a plain pointer.
    private var attentionJumpLabel: String {
        guard let target = attentionIndex, let m = messages[safe: target] else {
            return "needs attention"
        }
        return Fmt.deadlineChip(m.deadline)?.text.lowercased() ?? "needs attention"
    }

    var body: some View {
        ZStack {
            ReaderBackdrop()
                .ignoresSafeArea()

            column

            if let debugInfo {
                TriageDebugOverlay(info: debugInfo) { self.debugInfo = nil }
            }
            if let confirmMode {
                UnsubConfirm(
                    mode: confirmMode, senderName: senderName, busy: confirmBusy,
                    onConfirm: {
                        Task {
                            if confirmMode == .ask { await runUnsubscribe() } else { await runBlock() }
                        }
                    },
                    onCancel: { if !confirmBusy { self.confirmMode = nil } })
            }
        }
        .keyContext(.thread)
        .keyBindings(.thread, bindings)
        .task(id: threadId) {
            await load()
            // Only once the thread is HERE: the hand-off names a message, and
            // whether it is in this thread is not knowable until it has loaded.
            consumePendingReply()
            await refreshOpens()
        }
        .task(id: newestSender) { await refreshUnsub() }
        // SIZE THE THREAD UP BEFORE IT IS SCROLLED THROUGH. Nothing here waits
        // on it: the pass runs off screen, fills the height memory message by
        // message, and the reader simply finds the sizes already there when it
        // gets to them. Keyed on the width too — a resized column is a
        // different set of heights and has to be taken again.
        .task(
            id: MeasurePass(
                thread: threadId, count: messages.count, width: bodyWidth, style: style,
                ready: styleReady)
        ) {
            // A window resize walks the width a pixel at a time, and each step
            // is a different set of heights — so this task is restarted a
            // hundred times during one drag. Waiting a beat first turns that
            // into one pass at the width the drag ended on. It also lets the
            // reader paint before anything starts measuring behind it.
            try? await Task.sleep(for: .milliseconds(120))
            guard !Task.isCancelled, styleReady, bodyWidth > 0, !messages.isEmpty else { return }
            let sized = messages
            let measuring = style
            FrameMeasurer.shared.measure(
                sized, width: bodyWidth, viewport: viewportHeight, style: measuring,
                allowRemote: prefs.loadRemoteImages, token: threadId
            ) {
                // THE RAIL SWAPS ONCE, when every message is a measurement
                // rather than a guess — see `marks(for:)`.
                marks = Self.marks(for: sized, style: measuring)
            }
        }
        // The pass outlives nothing: a reader that has left owns no thread to
        // measure. Scoped by token because a thread SWITCH starts the next
        // pass before this fires.
        .onDisappear { FrameMeasurer.shared.cancel(token: threadId) }
        // The default changed in Settings while this thread is open. A thread
        // that has been switched by hand keeps what it was told — that is the
        // whole point of an override — so only the ones following the default
        // follow it here, and they follow it through the guess: switching TO
        // Automatic has to re-read the thread, not leave it on what the fixed
        // setting last drew.
        .onChange(of: prefs.threadStyle) { _, _ in
            guard styles.style(threadId) == nil else { return }
            style = resolvedStyle(messages)
        }
        // NEW MAIL IN THIS VERY THREAD, from the poll that heard about it.
        // `onChange` rather than `.task(id:)`: a task keyed on the token would
        // also fire on mount, refetching the thread `load()` is already
        // fetching.
        .onChange(of: store.openThreadRefreshToken) { _, _ in
            Task { await refreshInPlace() }
        }
        // Warm the NEXT queued thread while this one is being read, so e/d's
        // done+advance opens it instantly.
        .onAppear {
            if let cur = store.threadQueue.firstIndex(where: { $0.thread_id == threadId }),
                let next = store.threadQueue[safe: cur + 1]
            {
                ThreadPrefetch.shared.prefetch(next.thread_id)
            }
        }
    }

    // MARK: - the column

    /// Header, mail, composer — and WHERE the composer attaches is the one thing
    /// the two platforms disagree about.
    ///
    /// The Mac stacks it: a third row of the VStack, under the mail, never over
    /// it, so the message you are answering stays readable while you answer it.
    ///
    /// A phone cannot stack it, because a keyboard is about to take half the
    /// screen. In a VStack SwiftUI lifts the WHOLE stack clear of the keyboard,
    /// which shoves the header off the top and leaves the reader scrolled to
    /// nowhere. As a bottom safe-area inset the composer rides the keyboard on
    /// its own while the mail behind it merely gains bottom inset — it stays
    /// where it was, and scrolls under a bar that is now a bar.
    @ViewBuilder
    private var column: some View {
        #if os(macOS)
            VStack(spacing: 0) {
                header
                content
                composer
            }
        #else
            VStack(spacing: 0) {
                header
                content
            }
            .safeAreaInset(edge: .bottom, spacing: 0) { composer }
        #endif
    }

    /// MOUNTED UNCONDITIONALLY, and gated on `store.inlineReply` INSIDE. Reading
    /// the draft from this body instead would make every keystroke in the reply
    /// invalidate the entire reader — every message card, every sandboxed web
    /// frame, a full relayout of the column — for a change that only ever
    /// touches the bar at the bottom. (`@Observable` tracks the property, not the
    /// field, so there is no way to read the draft's target here cheaply.)
    ///
    /// With no draft open it renders nothing, which as a safe-area inset means
    /// no inset at all: the reader gets its full height back the moment the
    /// composer closes.
    private var composer: some View {
        InlineReply(
            messages: thread?.messages ?? [], threadSubject: thread?.subject ?? "",
            onEchoed: { Task { await reloadAfterSend() } })
    }

    // MARK: - chrome

    /// A Mac reads the whole header as the shared TOP BAR: the subject sits on
    /// the traffic lights' line the way every page title does, the participants
    /// ride to its right, and the actions take the far end. The subject pays
    /// for the fixed strip with truncation — one line, ellipsized — and the
    /// full text stays a copy-click away. A phone has no such width — four
    /// text buttons beside a subject would squeeze it to an ellipsis — so the
    /// same pieces stack, the actions under the line they act on, and leaving
    /// belongs to the navigation bar above rather than to a button of our own.
    private var header: some View {
        #if os(macOS)
            HStack(alignment: .firstTextBaseline, spacing: 12) {
                subjectLine(lines: 1)
                if !participantLine.isEmpty {
                    participantsText
                }
                Spacer(minLength: 12)
                actions
                Button { store.closeThread() } label: {
                    HStack(spacing: 4) {
                        Kbd("esc")
                        Text("back").font(Typo.micro)
                    }
                }
                .buttonStyle(.textAction)
            }
            // These metrics match every other page's bar: the reader's rule
            // ends on the line the rail's top edge is cut to.
            .padding(.horizontal, 24)
            .frame(height: TopBar.height)
            .overlay(alignment: .bottom) { Hairline() }
        #else
            VStack(alignment: .leading, spacing: 10) {
                titleBlock
                HStack(spacing: 16) {
                    actions
                    Spacer(minLength: 0)
                }
            }
            .padding(.horizontal, 18)
            .padding(.top, 2)
            .padding(.bottom, 12)
            .overlay(alignment: .bottom) { Hairline() }
        #endif
    }

    #if !os(macOS)
        private var titleBlock: some View {
            VStack(alignment: .leading, spacing: 3) {
                subjectLine(lines: 2)
                if !participantLine.isEmpty {
                    participantsText
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    #endif

    /// The subject IS the copy affordance — no icon earns a place in this
    /// header for a once-in-a-while verb. `lines` is the platform's call: one
    /// inside the Mac's fixed bar, two in the phone's stacked block.
    private func subjectLine(lines: Int) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Button {
                if let subject = thread?.subject, !subject.isEmpty {
                    Clip.copy(subject, flashing: $subjectCopied)
                }
            } label: {
                Text(thread?.subject ?? "…")
                    .font(Typo.serif(19, weight: .medium))
                    .foregroundStyle(Palette.ink)
                    .lineLimit(lines)
                    .truncationMode(.tail)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help("copy subject")
            if subjectCopied {
                Text("copied!")
                    .font(Typo.micro)
                    .foregroundStyle(Palette.positive)
                    .transition(.opacity)
            }
        }
        .animation(.easeOut(duration: 0.18), value: subjectCopied)
    }

    private var participantsText: some View {
        Text(participantLine)
            .font(Typo.rowSub)
            .foregroundStyle(Palette.inkFaint)
            .lineLimit(1)
            .truncationMode(.tail)
    }

    /// EVERY ACTION HERE NEEDS THE THREAD: they act on its newest message and
    /// they all call the daemon. With nothing loaded they are a row of controls
    /// that silently do nothing, offered next to an error saying the server is
    /// unreachable. Only `back` still means something, and it is not here.
    @ViewBuilder
    private var actions: some View {
        if thread != nil {
            // REPLY NEEDS A TARGET ON A PHONE. On the Mac `r` is the door and it
            // is always open; here the only other way in is a leading swipe from
            // a list, which lands you in this reader with the composer already
            // up. Without this, a thread opened by tapping it could be read and
            // never answered. Hidden while a composer IS open — it would be a
            // button that does nothing (`openInlineReply` refuses to reset a live
            // draft, and rightly).
            #if !os(macOS)
                if store.inlineReply == nil {
                    Button { openReply() } label: {
                        HStack(spacing: 4) {
                            Image(systemName: "arrowshape.turn.up.left")
                                .font(.system(size: 10, weight: .semibold))
                            Text("reply").font(Typo.micro)
                        }
                    }
                    .buttonStyle(.textAction)
                    .foregroundStyle(Palette.accent)
                }
            #endif
            // JUMP TO WHAT SURFACED THE THREAD: shown only while the
            // attention-bearing message is not the selected one — a
            // highlight below the fold helps nobody. Selecting it scrolls
            // (the index watcher owns the animation).
            if let target = attentionIndex, target != index {
                Button { index = target } label: {
                    HStack(spacing: 4) {
                        // The thread reads in order and opens at the END of it,
                        // so the obligation is usually BEHIND you. The arrow has
                        // to say which way it is, not always point down.
                        Image(
                            systemName: target > index
                                ? "arrow.down.to.line.compact" : "arrow.up.to.line.compact"
                        )
                        .font(.system(size: 10, weight: .semibold))
                        Text(attentionJumpLabel).font(Typo.micro)
                    }
                }
                .buttonStyle(.textAction)
                .foregroundStyle(
                    messages[safe: target]?.tier == .pastDue ? Palette.danger : Palette.warn
                )
                .help(messages[safe: target]?.one_line ?? "jump to the message that needs attention")
            }

            if prefs.developerMode {
                Button("triage debug") { Task { await openDebug() } }
                    .buttonStyle(.textAction).font(Typo.micro)
                Button(retriaging ? "re-triaging…" : "re-triage") {
                    Task { await retriageThis() }
                }
                .buttonStyle(.textAction).font(Typo.micro)
                .disabled(retriaging)
                .help("dev: reset this email's LLM verdicts and re-run triage")
            }

            Button { openSenderRule() } label: {
                HStack(spacing: 4) {
                    // The key chip is the Mac's promise that a key does this.
                    // A phone has no keyboard to promise anything to.
                    #if os(macOS)
                        Kbd("t")
                    #endif
                    Text("new rule").font(Typo.micro)
                }
            }
            .buttonStyle(.textAction)
            .help("write a rule for this sender — shows the ones already in effect")

            Button {
                confirmMode = .ask
            } label: {
                if let unsub {
                    Text("unsubscribe requested \(Fmt.relAge(unsub.requested_at)) ago")
                        .font(Typo.micro)
                } else {
                    HStack(spacing: 4) {
                        #if os(macOS)
                            Kbd("u")
                        #endif
                        Text("unsubscribe").font(Typo.micro)
                    }
                }
            }
            .buttonStyle(.textAction)
            .help("unsubscribe from this sender")
        }
    }

    @ViewBuilder
    private var content: some View {
        if loading {
            centeredNote("loading thread…")
        } else if error != nil || thread == nil {
            failurePane
        } else if messages.isEmpty {
            centeredNote("no messages in this thread.")
        } else {
            ScrollViewReader { proxy in
                HStack(spacing: 0) {
                    #if os(macOS)
                        // Read-only: it draws where you are, and j/k move you.
                        ThreadMinimap(
                            map: map, marks: marks, selected: index,
                            viewport: viewportHeight)
                    #endif

                    ScrollView {
                        // spacing 0: each message owns its vertical rhythm, so the
                        // hairline between two lands mid-gap instead of hugging one.
                        LazyVStack(alignment: .leading, spacing: 0) {
                            ForEach(Array(messages.enumerated()), id: \.element.id) { i, m in
                                MessageCard(
                                    message: m, style: style, position: i,
                                    selected: i == index, ruled: i > 0,
                                    opens: opens[m.id] ?? []
                                ) {
                                    index = i
                                }
                                // THE CARD DIFFS ON ITS DATA, NOT ON ITS
                                // CALLBACK. Every card carries a fresh
                                // `onSelect` closure, and a closure minted in
                                // this body never compares equal to the last
                                // one — so without this, ANY invalidation of
                                // the reader (a row crossing the visibility
                                // threshold, receipts landing, the unsubscribe
                                // lookup returning) rebuilt every message card
                                // and re-ran every web frame's update, none of
                                // which had changed. See MessageCard's `==`.
                                .equatable()
                                .id(i)
                                // WHERE THIS MESSAGE IS IN THE WINDOW, which is
                                // the minimap's whole input — measured against
                                // the viewport rather than the document because
                                // a lazy stack does not have a document to
                                // measure against (see ThreadMinimap). Lands on
                                // the map object, not on `@State`, so scrolling
                                // does not re-render the reader.
                                .onGeometryChange(for: CGRect.self) {
                                    $0.frame(in: .scrollView)
                                } action: { frame in
                                    map.note(i, frame: frame)
                                    if i == newestIndex { newestHeight = frame.height }
                                }
                                .onDisappear { map.drop(i) }
                            }

                            Color.clear.frame(height: tailSpace)
                        }
                        // THE WIDTH THE MAIL LAYS OUT AT, taken from the column
                        // rather than derived from the constants around it: it
                        // is what every measured height is an answer FOR, and a
                        // number computed from three paddings in two files
                        // drifts the moment one of them changes. Measured
                        // INSIDE the padding, so it is the card's own width;
                        // the body is inset from that by the card's gutter.
                        .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { width in
                            columnWidth = width
                            // Declared the moment it is known, not when the
                            // measuring pass starts a beat later: the frames on
                            // screen are already reporting heights, and they
                            // would be filed against a width nobody had named
                            // and then thrown away by the pass that named it.
                            FrameHeights.shared.using(width: width)
                        }
                        .padding(.horizontal, Self.columnPadding)
                        .padding(.vertical, 4)
                        .frame(maxWidth: Self.columnWidth)
                        .frame(maxWidth: .infinity)
                        // INSIDE the scroll content deliberately: an overlay on the
                        // ScrollView itself would sit above it and eat the wheel
                        // wherever it covers, and the pointer parks in the gutter.
                        .overlay { gutterDismiss }
                    }
                    // THE READER OPENS AT THE END: the newest message is the one
                    // you came for, and with `tailSpace` under it the end of the
                    // scroll IS that message at the top of the window.
                    //
                    // `.initialOffset` and NOT the whole anchor: a bottom anchor
                    // that also governed size changes would keep the bottom edge
                    // pinned forever, and every web frame that measured itself
                    // late — anywhere in the thread — would shove the paragraph
                    // somebody is reading. Landing is a starting position, not a
                    // standing rule.
                    .defaultScrollAnchor(.bottom, for: .initialOffset)
                    // The minimap draws the whole thread, so the reader's own
                    // scroll bar would be a second, worse answer to the same
                    // question sitting right beside it.
                    .scrollIndicators(.hidden)
                    .onGeometryChange(for: CGFloat.self) { $0.size.height } action: {
                        viewportHeight = $0
                    }
                    // WHOSE SCROLL IS IT. Anything but idle means the reader has
                    // a hand on it — a wheel, a trackpad, the tail of a flick —
                    // and while that is true the landing below must keep its
                    // hands off. It fires twice a gesture, not per tick.
                    .onScrollPhaseChange { _, phase in
                        handScrolling = phase != .idle
                    }
                }
                // A STEP animates, A JUMP DOES NOT. j/k moves to the neighbouring
                // card, which is already laid out and reads as the mail sliding
                // under the cursor. A jump from the rail can be forty messages
                // away, and an animated scroll to a row a LAZY stack has never
                // instantiated is the one SwiftUI reliably declines to perform:
                // it has nothing to animate from, so it does nothing at all.
                .onChange(of: index) { was, now in
                    if abs(now - was) == 1 {
                        withAnimation(Motion.scrollFollow) { proxy.scrollTo(now, anchor: .top) }
                    } else {
                        settle(on: now, proxy: proxy)
                    }
                }
                // A refetch that APPENDED a message — a sent reply's own echo —
                // has to land on it, and so does a switch to another thread, where
                // this view stays mounted and only its content changes. Unanimated:
                // both are a starting position, not a movement.
                //
                // It scrolls to the SELECTION, not to the last row, because the two
                // are not always the same: `refreshInPlace` puts the selection back
                // on the message that was on screen, and a hardcoded landing here
                // would fire on that same update and undo the position it just
                // preserved.
                // THE LANDING: the top of the newest message at the top of the
                // window. The bottom anchor above only gets us to the END of the
                // scroll, which is a different place — for a message taller than
                // the window it is that message's BOTTOM edge — so the real
                // landing is taken here, once the newest card has a height.
                //
                // It watches the measurements and not `tailSpace`, which was the
                // bug: a newest message taller than the window leaves no room to
                // reserve, so tailSpace stays 0, never changes, and a watcher on
                // it never fires. The two numbers it is made of do change.
                .onChange(of: [newestHeight, viewportHeight]) { was, now in
                    guard newestHeight > 0, index == newestIndex else { return }
                    // Land once per thread, then only while the reader is still
                    // parked on the newest — a resize, or the card growing as its
                    // images arrive. Somebody who has scrolled back into the
                    // history is not holding a position this may correct.
                    //
                    // NOR IS SOMEBODY WHO IS SCROLLING RIGHT NOW. "Parked" allows
                    // a couple of dozen points of slack, which is about one turn
                    // of a wheel — so the first nudge off the newest message used
                    // to be answered by a height landing somewhere in the thread
                    // and yanking the reader back to where they started. A
                    // gesture in progress owns the scroll; the window resizing
                    // (the third arm) is not a gesture and still lands.
                    guard !landed || (parkedOnNewest && !handScrolling) || was.last != now.last
                    else { return }
                    proxy.scrollTo(newestIndex, anchor: .top)
                    landed = true
                }
                .onChange(of: messages.last?.id) { _, _ in
                    proxy.scrollTo(index, anchor: .top)
                }
                // A STYLE FLIP IS THE WHOLE COLUMN RELAID OUT UNDER SOMEBODY'S
                // EYES. Every html message re-measures under the other style's
                // cache key (see ThreadStyle.frameKey), so for a beat the thread
                // is a stack of empty placeholders and the message being read is
                // nowhere near where it was. Nothing else here re-aims: the
                // landing above only fires for a reader parked on the newest, and
                // the selection has not moved, so this is the one hand on it.
                //
                // A LONGER LEASH than a rail jump, and it holds: those heights
                // arrive out of a web frame long after the flip, and each one that
                // lands shifts the target again — so being on target once is not
                // being finished. A plain-text thread reports nothing late, is on
                // target from the first check, and is done in three.
                //
                // IT AIMS WHERE THE READER IS, not at the selection — the same
                // distinction `refreshInPlace` makes, for the same reason: a
                // wheel scroll moves what is on screen without ever touching
                // the selection, and re-aiming at the selection would use the
                // flip to yank a wheel reader back to the newest. The selection
                // itself stays put, so `settle` is told which selection it is
                // riding under rather than assuming it is the target.
                //
                // Only for a thread that has already landed. Before that, the
                // style is being resolved by the load that is about to land it,
                // and two hands on the scroll is a fight.
                //
                // KNOWN LIMITATION: a reader deep inside a NEWEST message that
                // is taller than the window has no row to name — `anchorId` is
                // nil there, because the selection is the newest and the topmost
                // visible row is that same message — so the flip re-lands them at
                // its top rather than at the paragraph they were on. Anchoring
                // finer than a message would mean asking the web frame where the
                // reader is inside its document, which the frame is not asked
                // anything else.
                .onChange(of: style) { _, _ in
                    // THE RAIL IS DRAWN TO THE STYLE'S MEASURE, so a flip makes
                    // every nub the wrong length until it is redrawn. Taken
                    // here rather than waiting on the measuring pass: the pass
                    // has a whole set of heights to take under the new style's
                    // keys, and a rail that is wrong for a second reads as a
                    // rail that jumps.
                    marks = Self.marks(for: messages, style: style)
                    guard landed else { return }
                    let target = anchorId.flatMap { id in messages.firstIndex { $0.id == id } } ?? index
                    settle(
                        on: target, proxy: proxy, tries: 24, every: .milliseconds(40), hold: 3,
                        under: index)
                }
                // The style radio rides the mail's own top-right corner rather
                // than the header row: it is a verdict about the mail below it,
                // and the header is already a sentence of verbs.
                .overlay(alignment: .topTrailing) {
                    StyleRadio(style: style, ready: styleReady) { chooseStyle($0) }
                        .padding(.top, 10)
                        .padding(.trailing, 12)
                }
            }
        }
    }

    /// EMPTY ROOM UNDER THE LAST MESSAGE, and it is the whole reason the newest
    /// email can sit at the TOP of the window instead of the bottom of it: a
    /// scroll cannot travel past its own content, so with nothing below it the
    /// last message comes to rest against the bottom edge no matter what it is
    /// asked to do. The room is exactly what the viewport has left over once that
    /// message is in it, so the scroll ends the instant the message is at the top
    /// — never a pixel of slack beyond that.
    ///
    /// Nothing for a one-message thread: there is no history above it to scroll
    /// back into, so dead space below would be dead space for its own sake. And
    /// nothing until both halves are measured, which keeps the very first layout
    /// from opening on a screenful of nothing.
    /// A JUMP, TAKEN UNTIL IT LANDS. One `scrollTo` into a LAZY stack is a guess:
    /// the rows between here and there have never been laid out, so the scroll
    /// aims with estimated heights, and the real ones — a web frame measuring
    /// itself a beat after it is placed — move the target out from under the
    /// landing. That is why a click on the rail took you near the right message
    /// rather than to it.
    ///
    /// The card's own live frame is the feedback: while its top edge is not the
    /// top of the window, aim again. Bounded, because a thread whose heights
    /// never settle must cost a fifth of a second and not a spin, and abandoned
    /// the moment the selection moves — somebody who has moved on owns the scroll.
    ///
    /// `hold` is how many checks in a row must find the card on target before the
    /// loop believes it. One is right for a jump, where arriving IS the job. A
    /// relayout of the whole column (a style flip) needs more: the first frames
    /// to re-measure can put the target at the top on their own, and the ones
    /// still to come would then move it with nobody watching.
    ///
    /// `under` is the selection this settle is riding beneath when the target is
    /// not the selection itself — a style flip anchors on the message on SCREEN,
    /// which for a wheel reader is not the selected one. The loop is still
    /// abandoned the moment the selection moves off it, because somebody who has
    /// moved on owns the scroll; left nil, the target is the selection, as it is
    /// for every jump.
    ///
    /// ONE SETTLE AT A TIME: each call ends the previous loop first. Two live
    /// loops can hold the same selection and different targets — a flip while a
    /// rail jump is still landing — and would issue competing `scrollTo`s on
    /// alternating ticks. And THE WHEEL ENDS IT TOO: the selection guard cannot
    /// see a wheel scroll, so on macOS each loop watches for one and stands
    /// down — the reader's own hand always outranks the aim. The watch is the
    /// loop's alone and dies with it, so a loop ending late can never take a
    /// newer loop's watch down with it.
    private func settle(
        on target: Int, proxy: ScrollViewProxy, tries: Int = 8,
        every: Duration = .milliseconds(25), hold: Int = 1, under selection: Int? = nil
    ) {
        settleTask?.cancel()
        proxy.scrollTo(target, anchor: .top)
        settleTask = Task { @MainActor in
            let wheel = WheelYield()
            wheel.watch()
            defer { wheel.end() }
            var steady = 0
            for _ in 0..<tries {
                do { try await Task.sleep(for: every) } catch { return }
                guard !wheel.tripped, index == (selection ?? target) else { return }
                if let frame = map.frames[target], abs(frame.minY) < 2 {
                    steady += 1
                    if steady >= hold { return }
                    continue
                }
                steady = 0
                proxy.scrollTo(target, anchor: .top)
            }
        }
    }

    /// Whether the newest message's top edge is still sitting at the top of the
    /// window — that is, whether the reader is where the landing put them. Read
    /// off the card's own live frame rather than from a visibility fraction: a
    /// message ten screenfuls tall is barely "visible" by area and is still
    /// exactly the thing being read.
    private var parkedOnNewest: Bool {
        guard let frame = map.frames[newestIndex] else { return false }
        return abs(frame.minY) < 24
    }

    private var tailSpace: CGFloat {
        guard messages.count > 1, viewportHeight > 0, newestHeight > 0 else { return 0 }
        return max(0, viewportHeight - newestHeight - 24)
    }

    /// One mark per message for the rail LEFT of the mail: WHO WROTE IT, which is
    /// the whole colour code and the same colour their avatar carries in the
    /// bands; whether it is an obligation (said with width, so a deadline three
    /// messages up is visible from anywhere in the thread); and how long the
    /// message is about to be, which is what holds the map still for the mail the
    /// lazy stack has not laid out yet.
    ///
    /// Derived WHEN THE THREAD LANDS (see `adopt`) and not on every render: it
    /// is a walk of every message's text, and it was being re-walked whenever
    /// anything at all invalidated the reader — a row crossing the visibility
    /// threshold, which happens repeatedly while you scroll. The messages are
    /// the only input, and they only change when a fetch replaces them.
    private static func marks(
        for messages: [ClientMessage], style: ThreadStyle
    ) -> [ThreadMinimap.Mark] {
        // ALL MEASURED OR NONE, and that is not fussiness. A map drawn half
        // from measurements and half from guesses redraws itself as each
        // measurement lands, which is a rail that crawls while you read it. The
        // measuring pass fills the whole set and says so once, so the rail
        // changes scale exactly once per thread — and on the next open there is
        // nothing left to change.
        let sizes = messages.map { measuredCard($0, style: style) }
        let complete = sizes.allSatisfy { $0 != nil }
        return messages.enumerated().map { i, m in
            ThreadMinimap.Mark(
                attention: m.needsAttention,
                tint: Palette.avatarColors(for: m.senderString).fg,
                estimate: (complete ? sizes[i] : nil) ?? mapEstimate(m, style: style))
        }
    }

    /// This message's card at the size it really draws: the body as WebKit
    /// measured it, plus the chrome the card puts around it. nil while nobody
    /// has measured it.
    ///
    /// A PLAIN-TEXT message counts as measured. It has no web frame to measure
    /// — and it is the one shape the guess is good at, being a count of lines
    /// of text in a message that is nothing but lines of text. Holding the
    /// whole rail hostage to a body no engine will ever be asked to lay out
    /// would mean a thread with one plain reply never gets a true map at all.
    private static func measuredCard(_ m: ClientMessage, style: ThreadStyle) -> CGFloat? {
        guard let html = m.html, !html.isEmpty else {
            // Its own guess, not `mapEstimate`: this card draws with its quoted
            // history collapsed behind a chip, exactly as the html ones do, and
            // the rail must not draw it at the length of the chain it is
            // answering.
            return MinimapGeometry.card(
                bodyHeight: MessageCard.guessedBody(m, style: style),
                attachments: m.attachmentList.count, style: style)
        }
        // Under the STYLE's own key: the same message has one measurement per
        // measure it was laid out at, and a card's height on a bubble rail is
        // the wrong length of nub.
        guard let body = FrameHeights.shared.get(style.frameKey(m.id)) else { return nil }
        return MinimapGeometry.card(
            bodyHeight: body, attachments: m.attachmentList.count, style: style)
    }

    /// WHAT THIS MESSAGE IS WORTH ON THE RAIL, from its own text and nothing
    /// else. Deliberately NOT the height the web frame measured for it, even
    /// though the reader keeps those (FrameHeights, for painting a reopened
    /// message at its final size): a map that mixes measured cards with guessed
    /// ones redraws itself as the measurements land, which is a rail that crawls
    /// while you read it. A map that is uniformly approximate holds still, and
    /// holding still is the whole job.
    private static func mapEstimate(_ m: ClientMessage, style: ThreadStyle) -> CGFloat {
        MinimapGeometry.estimate(
            text: m.content, html: m.html, attachments: m.attachmentList.count, style: style)
    }

    /// THE WIDTH A BODY IS ACTUALLY LAID OUT AT, which is not the column's: a
    /// card is inset by its rule gutter on the leading side alone, while a
    /// bubble is capped at its own narrower measure and inset on both. The
    /// measuring pass has to render at exactly this or every height it takes is
    /// an answer to a different question.
    private var bodyWidth: CGFloat {
        guard columnWidth > 0 else { return 0 }
        switch style {
        case .classic: return max(0, columnWidth - MessageCard.bodyInset)
        case .bubbles:
            return max(0, min(columnWidth, Self.bubbleWidth) - MessageCard.bodyInset * 2)
        }
    }

    /// The mail's measure. The dismissible gutter is defined as the complement
    /// of it, so the two can never drift apart — and the inline composer pins
    /// itself to the same measure, so the reply sits under the column it answers.
    static let columnWidth: CGFloat = 900

    /// The widest a chat bubble is ever drawn. Narrower than the column on
    /// purpose: what makes a conversation readable as one is the empty margin
    /// opposite each side, and a bubble that fills the measure has none.
    static let bubbleWidth: CGFloat = 620

    /// The mail's own inset. A phone is narrower than the column will ever be,
    /// so the padding IS the measure there and it matches the header above it.
    #if os(macOS)
        private static let columnPadding: CGFloat = 22
    #else
        private static let columnPadding: CGFloat = 18
    #endif

    /// CLICK BESIDE THE MAIL TO LEAVE IT — the same exit as Esc.
    ///
    /// Only the two strips FLANKING the column take hits: the middle is exactly
    /// the column's own footprint and is inert, so a click on a message card
    /// still selects it, and a link, button or selection inside a web frame is
    /// never intercepted. In a window narrower than the full measure the strips
    /// collapse to nothing, which is right — there is no gutter to click.
    ///
    /// NOTHING ON A PHONE: the screen is narrower than the column, so the strips
    /// would be zero-width anyway — and a tap-to-leave target laid over a
    /// reading surface answers a gesture the navigation bar and the edge swipe
    /// already own.
    @ViewBuilder
    private var gutterDismiss: some View {
        #if os(macOS)
            HStack(spacing: 0) {
                gutterStrip
                Color.clear
                    .frame(width: Self.columnWidth)
                    .allowsHitTesting(false)
                gutterStrip
            }
        #endif
    }

    #if os(macOS)
        private var gutterStrip: some View {
            Color.clear
                .frame(maxWidth: .infinity)
                .contentShape(Rectangle())
                .onTapGesture {
                    // Every modal here owns a full-window scrim and already takes
                    // the click before this layer sees it. The guard is what keeps
                    // that true if one ever stops doing so: dismissing a dialog must
                    // never also throw away the email behind it.
                    guard confirmMode == nil, debugInfo == nil, !store.modalOverlayOpen else {
                        return
                    }
                    // An unsent draft is not something a stray click beside the mail
                    // gets to destroy — there is no undo for a lost reply. Esc leaves
                    // the composer first, then the email.
                    guard store.inlineReply == nil else { return }
                    store.closeThread()
                }
        }
    #endif

    private func centeredNote(_ text: String, tone: Color = Palette.inkFaintest) -> some View {
        Text(text)
            .font(Typo.rowSub)
            .foregroundStyle(tone)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    /// A thread that will not load is a DEAD END, and the reader navigated into
    /// it deliberately — so it owes them what went wrong and a way out, not one
    /// line of red text adrift in an empty window. Same shape as
    /// `DaemonDownPane`, because this is usually the same outage seen from
    /// inside the reader.
    ///
    /// The daemon being down is called out separately from a thread that simply
    /// failed: "the mail is on your machine, the thing that serves it isn't
    /// answering" is a materially different problem from "this email is broken",
    /// and only one of them is worth waiting out.
    private var failurePane: some View {
        let down = store.daemonDown
        return VStack(spacing: 14) {
            Image(systemName: down ? "bolt.horizontal.circle" : "exclamationmark.triangle")
                .font(.system(size: 30, weight: .light))
                .foregroundStyle(down ? Palette.warn : Palette.danger)

            Text(down ? "can't reach the squelch daemon" : "couldn't open this email")
                .font(Typo.serif(26, weight: .medium))
                .foregroundStyle(Palette.ink)

            Text(
                down
                    ? "This email is already on your machine — the daemon that serves it isn't answering. Is squelchd running? Retrying every 10 seconds."
                    : (error ?? "The server didn't return this thread.")
            )
            .font(.system(size: 13))
            .foregroundStyle(Palette.inkFaint)
            .multilineTextAlignment(.center)
            .frame(maxWidth: 380)

            HStack(spacing: 10) {
                Button {
                    Task { await load() }
                } label: {
                    Label("try again", systemImage: "arrow.clockwise")
                        .font(.system(size: 12, weight: .medium))
                        .symbolEffect(.rotate, isActive: loading)
                }
                .buttonStyle(.glass)
                .disabled(loading)

                Button { store.closeThread() } label: {
                    HStack(spacing: 5) {
                        #if os(macOS)
                            Kbd("esc")
                        #endif
                        Text("back").font(.system(size: 12, weight: .medium))
                    }
                }
                .buttonStyle(.glass)
            }
            .padding(.top, 4)
        }
        .padding(38)
        .passbandGlass(.pane, cornerRadius: 22, tint: down ? Palette.warnSoft : Palette.dangerSoft)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    // MARK: - keymap

    private var bindings: [KeyBinding] {
        [
            // allowInInput matters: a search input underneath may still hold
            // focus (if our focus-steal loses the race).
            // With a side panel open beside the reader, Esc sheds the PANEL and
            // keeps reading — the email is what you chose, the results are what
            // you came through. A second Esc closes the reader.
            KeyBinding("Escape", "back", allowInInput: true) {
                if store.sideView.isOpen {
                    store.closeSide()
                } else {
                    store.closeThread()
                }
            },
            // ⌘[ = back, same as Esc — the viewer is a page you navigated into.
            KeyBinding("[", "back", meta: true) { store.closeThread() },
            // The vim pair lost its left half: `h` means "remind" on every
            // surface of the app, and one letter that parks mail on two pages
            // and steps backward on a third would be a misfire on the exact key
            // where a misfire costs an email. ArrowLeft still steps back; `l`
            // stays because nothing else wants it.
            KeyBinding("l", "next email") { stepQueue(1) },
            KeyBinding("ArrowLeft", "prev email") { stepQueue(-1) },
            KeyBinding("ArrowRight", "next email") { stepQueue(1) },
            // DOWN THE STACK IS FORWARD IN TIME now that the thread reads in
            // order, so j and the down arrow move to the NEWER message — the
            // same direction the eye travels. The selection always comes to
            // rest at the top of the window (see `tailSpace`), so a step is
            // "put the next message in front of me", not "nudge the scroll".
            KeyBinding("j", "newer message") { stepMessage(1) },
            KeyBinding("k", "older message") { stepMessage(-1) },
            KeyBinding("ArrowDown", "newer message") { stepMessage(1) },
            KeyBinding("ArrowUp", "older message") { stepMessage(-1) },
            // The reading surface is where a miscategorized email is most
            // obvious, so correcting it should not require going back.
            KeyBinding("v", "fix triage") {
                guard let thread, let m = messages[safe: index] else { return }
                store.openTriageFix(
                    TriageFixTarget(
                        messageId: m.id, sender: m.from_addr, subject: thread.subject))
            },
            // `f` = FORWARD, and only in here. On a list row `f` is still
            // search-this-sender: a row is a sender you are sizing up, an open
            // email is a thing you might want to pass on, and the reader is the
            // only surface where "this message" is unambiguous enough to
            // forward without asking which one.
            //
            // The SELECTED message, not the thread's newest — the same rule `v`
            // keeps. On a back-and-forth the two differ, and j/k is how you
            // chose the one in front of you; forwarding something else because
            // it happened to arrive last would be silently sending the wrong
            // mail, which no undo covers.
            //
            // Into the PANE composer rather than the reader's inline slot: a
            // forward starts a new thread, so it needs a recipient (nothing
            // derives one) and a subject line, and the inline reply has neither
            // field. The two draft slots are independent, so a reply
            // half-written underneath is untouched.
            KeyBinding("f", "forward") {
                guard let m = messages[safe: index] else { return }
                // THE WHOLE MESSAGE goes over, not a handful of fields off it:
                // the composer renders this same message underneath the note,
                // with the reader's own body view, so what you are passing on is
                // on screen while you write the covering line. Only its id
                // reaches the wire. `thread?.subject` rides along as the
                // FALLBACK title for a daemon too old to send per-message
                // subject headers; `openComposeForward` resolves the two.
                store.openComposeForward(message: m, fallbackSubject: thread?.subject ?? "")
            },
            // `s` = the search `f` used to be, moved rather than dropped: the
            // sender lookup is worth a key in here, it just is not worth THE
            // key. The SELECTED message's sender, not the thread's newest: on a
            // back-and-forth the two differ, and the one you are looking at is
            // the one you mean. Search opens as the strip beside the reader, so
            // this does not cost you the email you are reading.
            KeyBinding("s", "search this sender") {
                guard let m = messages[safe: index] else { return }
                store.openSearch(seed: "from:\(m.from_addr)")
            },
            // THE PAIR: `e`/`d` finish this email and leave, `E`/`D` finish it
            // and open the next one in the queue. One letter, one verb, on every
            // surface — the shifted twin is the SAME verb with the walk attached,
            // not a different one, which is why it is a case away rather than a
            // key away.
            //
            // Plain done CLOSES even when there is a queue behind it. Finishing
            // an email is not a commitment to read the next one, and a reader
            // that hauls in more mail on the key you press to be rid of the mail
            // in front of you is the app arguing with you.
            KeyBinding("e", "done") { Task { await doneAndClose() } },
            KeyBinding("d", "done") { Task { await doneAndClose() } },
            KeyBinding("E", "done + next") { Task { await doneAndNext() } },
            KeyBinding("D", "done + next") { Task { await doneAndNext() } },
            // Plain `h`, the same key as every other surface — it took "prev
            // email"'s key (see the queue block above) rather than moving to
            // `H`, which would have made remind a verb you can ONLY reach
            // shifted. `E`/`D` above are a different thing: the shift is an
            // extra clause on a key that still means the same verb unshifted.
            KeyBinding("h", "remind + next") {
                guard let thread, let m = remindable else { return }
                store.openRemind(
                    RemindTarget(
                        messageId: m.id, sender: m.from_addr, subject: thread.subject,
                        remindAt: store.update(id: m.id)?.remind_at,
                        // The reader leaves the same way it does on done + next:
                        // the mail is dealt with, so the walk carries on.
                        onScheduled: { Task { await remindAndNext() } }))
            },
            KeyBinding("u", "unsubscribe") { confirmMode = .ask },
            // `r` = reply, and it lives HERE rather than in the composer's own
            // set because it is what opens the composer when there is none. With
            // one open the body has focus, so `r` is input-suppressed (no
            // allowInInput) and types a character, which is what it should do
            // inside a text field.
            KeyBinding("r", "reply") { openReply() },
            // Enter = the same reply, addressed to EVERYONE on the parent.
            //
            // Deliberately NOT allowInInput: with the composer's body focused
            // Enter is a newline and must stay one, and a search field that
            // still holds focus beside the reader keeps its own Enter too. The
            // composer's declining Enter (registered after ours, so it is asked
            // first) passes the key down here in edit phase — that is the one
            // path that reaches this handler with a draft open, and DECLINING
            // there (rather than swallowing) is what lets the key keep falling:
            // a no-op that consumes Enter would also eat the Return a focused
            // button in the error state is waiting for. There is no undo for a
            // lost reply, so an open draft is never replaced.
            KeyBinding(declining: "Enter", "reply all") {
                guard store.inlineReply == nil, newest != nil else { return false }
                openReplyAll()
                return true
            },
            // `b` = the shape of the thread, cards or bubbles. Per-thread, and
            // the last word: whatever Settings or the guess said, this is what
            // this thread is from now on.
            KeyBinding("b", "chat / email style") { toggleStyle() },
            // `t` = tune sender rule, same as on a list row: one verb, one key
            // everywhere. The target differs (the thread's sender rather than the
            // selected row's) but that is the only sender in view here. This
            // frees `r` for reply.
            KeyBinding("t", "new sender rule") { openSenderRule() },
            // A new message from the reader, in the modal composer over it. The two
            // draft slots are independent, so this cannot disturb an inline reply
            // open underneath — and with one open the body has focus, so `c` is
            // input-suppressed and types a letter instead.
            KeyBinding("c", "new message") { store.openComposeNew() },
        ]
    }

    /// Move the selection by one message, clamped at both ends. The scroll
    /// follows from the selection, never the other way round.
    private func stepMessage(_ delta: Int) {
        guard !messages.isEmpty else { return }
        index = min(newestIndex, max(0, index + delta))
    }

    /// `b` walks the radio: same choice, keyboard spelling.
    private func toggleStyle() { chooseStyle(style.flipped) }

    /// The radio and `b`, which are the same answer. It is kept for THIS thread
    /// and PINNED THERE, even when it agrees with the default of the moment:
    /// with Automatic in Settings the default is re-read on every open, so a
    /// thread left following it would go back to the guess the next time a
    /// reply changes what the guess says. A reader who has answered the
    /// question should not be asked it again.
    ///
    /// AND IT IS A NO-OP UNTIL THERE IS A THREAD TO ANSWER FOR. `style` holds
    /// its placeholder `.classic` until `adopt` resolves it, and the pin is for
    /// good — the ledger has no un-answering — so a press into the load window
    /// would pin the placeholder, for a thread nobody has read yet, and nothing
    /// would ever ask again.
    private func chooseStyle(_ chosen: ThreadStyle) {
        guard styleReady else { return }
        styles.set(threadId, chosen)
        guard style != chosen else { return }
        style = chosen
    }

    /// Whether `style` is THIS thread's resolved answer rather than the state's
    /// starting value or another thread's leftover. Asked of `styledFor`, not of
    /// the load flag: a switch to another thread keeps this view mounted with
    /// the previous answer in `style`, and the prefetch-cache path of `load`
    /// never raises `loading` at all — the id is the only thing that says which
    /// thread the answer in hand belongs to.
    ///
    /// The header button disables on the same test, so both doors to the flip
    /// say the same thing.
    private var styleReady: Bool { thread != nil && styledFor == threadId }

    /// HOW THIS THREAD IS DRAWN, in the one order that matters: what the reader
    /// said about THIS thread, then what Settings says about every thread, and
    /// only where that says Automatic, what the thread itself looks like.
    ///
    /// Answered from the messages that are in hand rather than from the state,
    /// because the caller is `adopt`, which is holding the thread it is about to
    /// install and has not installed it yet.
    private func resolvedStyle(_ messages: [ClientMessage]) -> ThreadStyle {
        if let pinned = styles.style(threadId) { return pinned }
        if let fixed = prefs.threadStyle.fixed { return fixed }
        // The participation veto, asked BEFORE the samples exist: it reads
        // `is_sent` alone, and the thread that fails it — which is most mail —
        // should not pay for quote-splitting and markup-scanning every body it
        // was never going to draw as chat.
        guard ThreadStyle.participated(messages.map(\.is_sent)) else { return .classic }
        return ThreadStyle.automatic(messages.map(sample))
    }

    /// A message reduced to what the guess asks about. The fresh length is the
    /// message WITHOUT the history it is quoting — the same split PlainBody
    /// collapses behind the chip, because "how long is this message" and "how
    /// much of it is this message" are the same question. In UTF-8 bytes, for
    /// the reason `chatMedianBytes` gives.
    private func sample(_ m: ClientMessage) -> ThreadStyle.Sample {
        ThreadStyle.Sample(
            // `is_sent` and not `fromMe`: the accessor answers the drawing
            // question, where an unknown side is drawn as theirs. The guess
            // needs to know it is unknown.
            fromMe: m.is_sent,
            freshBytes: Quotes.splitText(m.content).visible.utf8.count,
            htmlHeavy: ThreadStyle.htmlHeavy(html: m.html, plain: m.content),
            sender: m.from_addr)
    }

    /// The rule composer for THIS thread's sender — the same request shape `t`
    /// uses on a list row, so there is one composer in the app, not two.
    private func openSenderRule() {
        guard let newestSender else { return }
        Actions.tune(sender: newestSender)
    }

    // MARK: - reply

    /// `r` — answer the NEWEST message, the same one `u` acts on and `e`/`d`
    /// resolve. That is what a reader means by "reply": the bottom of the thread,
    /// not whichever message j/k is parked on.
    private func openReply() {
        guard let newest else { return }
        store.openInlineReply(replyTo: newest.id)
    }

    /// Enter — the same reply as `r`, on the same newest message, addressed to
    /// everyone the parent reached. The composer is not re-addressable once
    /// open, so an already-open draft is left exactly as it is rather than
    /// silently becoming a reply-all (or being thrown away for one).
    private func openReplyAll() {
        guard store.inlineReply == nil, let newest else { return }
        store.openInlineReply(replyTo: newest.id, replyAll: true)
    }

    /// A hand-off from another surface: `r` on a list row navigates here and asks
    /// for the composer on the row's own message. One-shot — cleared whether or
    /// not it lands, so a stale request can never fire against a later thread.
    private func consumePendingReply() {
        guard let wanted = store.pendingReplyMessageId else { return }
        store.pendingReplyMessageId = nil
        // Must be OUR message. It is, by construction (we were opened with that
        // row's thread_id), and the only way to miss is a parent the thread view
        // does not return — in which case there is nothing to answer and `r`
        // remains one press away.
        guard thread?.messages.contains(where: { $0.id == wanted }) == true else { return }
        store.openInlineReply(replyTo: wanted)
    }

    /// Refetch after a send whose echo has landed, so the sent copy is IN the
    /// thread instead of appearing only after the next poll. The prefetch cache
    /// holds the PRE-send copy, so it is overwritten rather than read — otherwise
    /// reopening this thread would serve a version with the reply missing.
    private func reloadAfterSend() async {
        guard let view = try? await APIClient.shared.getThread(threadId) else { return }
        ThreadPrefetch.shared.note(threadId, view)
        adopt(view)
        // The echo is a new message id, so the receipt map has nothing for it
        // yet. Nothing has opened it a second after it went out — this is what
        // arms the mark for the poll that eventually finds one.
        await refreshOpens()
    }

    /// Refetch because the poller saw a newer message in this thread, WITHOUT
    /// moving the person reading it. `adopt` lands on the newest, which is right
    /// when a thread opens and wrong under somebody's eyes: they are three
    /// messages down reading, and the mail arriving is not a reason to take the
    /// page away.
    ///
    /// So the message on screen is remembered by ID and found again afterwards.
    /// The stack is newest-first, so everything below the arrivals shifts down
    /// by however many landed — an index restored as a NUMBER would silently
    /// mean a different message. Somebody already on the newest DOES follow it
    /// to the new one: that is the reply they were sitting there waiting for.
    ///
    /// Nothing to preserve before the first load lands, and `load()` is bringing
    /// the fresh copy anyway, so an empty viewer just lets it.
    private func refreshInPlace() async {
        guard thread != nil else { return }
        guard let view = try? await APIClient.shared.getThread(threadId) else { return }
        // Anchor AFTER the await: a reader who moved while the fetch was in
        // flight is anchored where they are now, not where they were.
        let anchor = anchorId
        // Same overwrite as the post-send reload: the cached copy predates the
        // arrival, and reopening this thread must not serve it back.
        ThreadPrefetch.shared.note(threadId, view)
        adopt(view)
        if let anchor, let found = messages.firstIndex(where: { $0.id == anchor }) {
            index = found
        }
        await refreshOpens()
    }

    /// The message the reader is actually ON, whether they got there with the
    /// keys or the wheel. A moved selection wins; failing that, the topmost
    /// VISIBLE row stands in, because a wheel scroll moves what is on screen
    /// without ever touching the selection — preserving the selection alone
    /// would let a background refresh yank a wheel reader back to the newest.
    /// nil means the reader really is sitting on the newest, which is the one
    /// case a refresh may move them: onto the reply they are waiting for.
    ///
    /// Asked of the MAP rather than of a visibility flag per row. The map is
    /// already tracking every mounted card's rectangle for the rail, and it
    /// does it in an object — where a per-row `onScrollVisibilityChange` wrote
    /// reader `@State`, so every message crossing the edge of the window
    /// re-rendered the whole reader for an answer nothing needed until a
    /// refresh landed.
    private var anchorId: Int? {
        if index != newestIndex { return messages[safe: index]?.id }
        guard let top = map.topmost, top < newestIndex else { return nil }
        return messages[safe: top]?.id
    }

    // MARK: - queue navigation

    /// HORIZONTAL queue nav: move between the queued emails WITHOUT resolving
    /// anything — the newsletter "2 this week" browse.
    private func stepQueue(_ delta: Int) {
        let queue = store.threadQueue
        guard queue.count > 1,
            let cur = queue.firstIndex(where: { $0.thread_id == threadId }),
            let next = queue[safe: cur + delta]
        else { return }
        store.openThread(next.thread_id, queue: queue)
    }

    /// e/d — "done": resolve the email in front of you and leave, whatever is
    /// or is not queued behind it. The same departure animation as done + next,
    /// because it is the same verb — the walk is the only difference.
    private func doneAndClose() async {
        let liftedAt = liftOff()
        await resolveOpenThread()
        await flightOut(since: liftedAt)
        // Same outlived-its-reader guard as the walk: if this finished after the
        // reader moved to another thread, closing would close THAT one.
        guard store.threadId == threadId else { return }
        store.closeThread()
    }

    /// The resolve both verbs share, undo-first through `Actions.done` whenever
    /// there is a row to hand it — the queue's own entry, which is the update
    /// the surface underneath is still showing.
    ///
    /// A reader with no queue behind it (search, a right-rail record, a push
    /// notification) has no row, so it resolves the newest message directly.
    /// There is no undo chip on that path: undo restores a row to a band, and
    /// this one never came from one.
    private func resolveOpenThread() async {
        if let row = store.threadQueue.first(where: { $0.thread_id == threadId }) {
            await Actions.done(row)
            return
        }
        guard let newest else { return }
        do {
            try await APIClient.shared.setStatus(newest.id, .done)
            // Same unpin as Actions.done — this path resolves the message
            // without going through it.
            await ImageStore.shared.release(messageId: newest.id)
            FrameHeights.shared.clear(messageId: newest.id)
            // And the same optimistic drop: the surface underneath is still
            // mounted, so without this the reader closes back onto a row for
            // mail that is already done.
            store.noteResolved(newest.id)
            store.pushToast("done", .info)
        } catch {
            store.pushToast(errText(error, "done failed"), .error)
        }
    }

    /// E/D — "done + next": mark the current thread's update done (keeping its
    /// 5s undo), then advance to the NEXT queued update in place; if none
    /// remain — or if this reader never had a queue behind it — close the
    /// viewer, which is exactly what plain `e` does.
    ///
    /// AND IT IS A MOTION, not a swap. The email you finished flies up and out,
    /// and then the next one comes in from below — the difference between "that
    /// one is dealt with, here is the next" and a screen that blinked. More mail
    /// from the SAME sender comes in from the RIGHT instead: a second newsletter
    /// off the same list is the next item in a pile, not the next subject, and
    /// the motion should say which of the two you are getting.
    ///
    /// TWO BEATS, EXPLICITLY TIMED, and driven by offsets rather than by a
    /// SwiftUI transition: a transition is the framework's to choose at the
    /// moment a view is inserted or removed, and when it declines to use yours it
    /// substitutes a crossfade. The departure runs concurrently with the round
    /// trip — the email is going either way — and only the leftover of its
    /// 220ms is ever waited on.
    private func doneAndNext() async {
        let queue = store.threadQueue
        let cur = queue.firstIndex(where: { $0.thread_id == threadId })
        // THE NEXT DIFFERENT EMAIL, not merely the next row. Every band listing
        // is one row per THREAD — the store partitions them that way, and
        // resolving one resolves the whole thread — so on those queues this
        // reads exactly like `cur + 1`. The REMINDER SCHEDULE is the exception
        // the store spells out: it lists one row per REMINDER, so two siblings
        // of one thread can both be on it, and stepping onto the second would
        // re-open the email just finished. `cur` would then find that same row
        // again, and the walk would never move for as long as the key is held.
        //
        // Already-resolved rows are skipped for the same reason: a queue is a
        // snapshot taken when the reader opened, and mail dealt with since (the
        // sibling the store just resolved, a row done from a list underneath) is
        // not something to walk a reader onto.
        let next = cur.flatMap { c in
            queue[(c + 1)...].first {
                $0.thread_id != threadId && !store.resolvedIds.contains($0.id)
            }
        }

        let liftedAt = liftOff()
        await resolveOpenThread()
        await flightOut(since: liftedAt)

        // The reader can be closed while the email is still leaving — Esc, or
        // another surface taking the screen. A done+next that outlived its own
        // reader must not haul the next thread back onto a page nobody is on.
        guard store.threadId == threadId else { return }

        guard let cur, let next else {
            // Nothing behind it — an empty queue, or the end of one: the reader
            // leaves with the email rather than snapping back to show an empty
            // one.
            store.closeThread()
            return
        }
        let edge: AppStore.ThreadEdge = sameSender(next, queue[cur]) ? .trailing : .bottom
        // Mounted OFF SCREEN, unanimated, and walked in on the next frame. The
        // sleep is the whole reason this works: an offset that is set and
        // cleared inside one update has never been drawn, so there is nothing
        // for the animation to move away from.
        store.openThread(next.thread_id, queue: queue, entering: edge)
        try? await Task.sleep(for: .milliseconds(30))
        withAnimation(Motion.deckCard) { store.threadFlight = .settled }
    }

    /// The tail of `doneAndNext`, for `h`: the reminder is already set (the
    /// palette did that, and the row is already gone from the bands), so this is
    /// only the departure and the walk. Both beats still run — the email leaving
    /// is what says the reminder took, and a reader that just sat there would
    /// read as a key that did nothing.
    private func remindAndNext() async {
        let queue = store.threadQueue
        let liftedAt = liftOff()
        await flightOut(since: liftedAt)
        // Outlived its own reader (Esc, another surface): do not haul a thread
        // back onto a page nobody is on.
        guard store.threadId == threadId else { return }

        guard let cur = queue.firstIndex(where: { $0.thread_id == threadId }),
            let next = queue[safe: cur + 1]
        else {
            store.closeThread()
            return
        }
        let edge: AppStore.ThreadEdge = sameSender(next, queue[cur]) ? .trailing : .bottom
        store.openThread(next.thread_id, queue: queue, entering: edge)
        try? await Task.sleep(for: .milliseconds(30))
        withAnimation(Motion.deckCard) { store.threadFlight = .settled }
    }

    /// Beat one: lift the finished email out through the top of the window, and
    /// hand back when it started so the wait can be the REMAINDER of the flight
    /// rather than the whole of it on top of the round trip.
    private func liftOff() -> ContinuousClock.Instant {
        withAnimation(Motion.depart) { store.threadFlight = .departing }
        return .now
    }

    /// Wait out whatever is left of the departure. Zero when the daemon took
    /// longer to answer than the email took to leave, which is the common case.
    private func flightOut(since start: ContinuousClock.Instant) async {
        let flown = ContinuousClock.now - start
        guard flown < Motion.departTime else { return }
        try? await Task.sleep(for: Motion.departTime - flown)
    }

    /// Same correspondent, by canonical address — the newsletter case, where
    /// "next" means another one off the same pile.
    private func sameSender(_ a: AttentionUpdate, _ b: AttentionUpdate) -> Bool {
        let canon = { (s: String) in s.trimmingCharacters(in: .whitespaces).lowercased() }
        return canon(a.sender) == canon(b.sender)
    }

    // MARK: - data

    private func load() async {
        // A fresh thread mounts fresh rows; where the LAST one's cards were
        // must not anchor this one, and its shape must not be what the minimap
        // draws for a thread this one knows nothing about yet.
        map.forget()
        newestHeight = 0
        landed = false
        // Fresh prefetch hit → render it and skip the round-trip entirely (the
        // cache is at most 60s old; e/d/refresh paths repopulate it).
        if let cached = ThreadPrefetch.shared.cached(threadId) {
            adopt(cached, opening: true)
            error = nil
            loading = false
            return
        }
        loading = true
        error = nil
        do {
            let view = try await APIClient.shared.getThread(threadId)
            ThreadPrefetch.shared.note(threadId, view)  // instant reopen
            adopt(view, opening: true)
        } catch {
            self.error = errText(error, "thread load failed")
        }
        loading = false
    }

    /// Take a loaded thread and derive its reader state. `opening` is true only
    /// when this is the thread ARRIVING — a refetch under somebody's eyes says
    /// false, which is what keeps the style from being re-decided while they
    /// read: the guess reads the messages, a reply changes the messages, and a
    /// column that redraws itself as chat because an answer landed is a column
    /// that moved the paragraph somebody was on.
    private func adopt(_ view: ClientThreadView, opening: Bool = false) {
        thread = view
        // THE STYLE FIRST: the rail below is drawn to its measure, so a mark
        // taken before the thread has chosen one is drawn for the wrong one.
        if opening {
            style = resolvedStyle(view.messages)
            styledFor = threadId
        }
        // And the one place the messages change is the one place the rail is
        // drawn.
        marks = Self.marks(for: view.messages, style: style)
        // LAND ON THE NEWEST. It is last in the stack now, and `tailSpace` is
        // what lets the scroll put it at the top of the window rather than the
        // bottom.
        index = max(0, view.messages.count - 1)
        // What the ⌘K agent is told it is looking at. Lifted into the store
        // because the ask bar is a modal above this view and cannot see its
        // state, and written HERE because this is the one place a thread lands
        // — `newest` so the agent targets exactly what `u`, `e` and the triage
        // inspector already do. Guarded because a slow fetch can land after the
        // user moved on: openThread(B) cleared the summary, and A's late adopt
        // must not put A's subject and message id back under B's thread id. The
        // id rides along regardless, so a reader can check rather than trust —
        // see AppStore.currentThreadSummary.
        if store.threadId == threadId {
            store.openThreadSummary = newest.map {
                OpenThreadSummary(
                    threadId: threadId, subject: view.subject.displaySubject,
                    newestMessageId: $0.id)
            }
        }
    }

    /// Read receipts for this thread's messages.
    ///
    /// Every message is asked about rather than only the ones the user sent.
    /// The wire does carry a "this copy is mine" flag now (`is_sent`), but it is
    /// ABSENT on an older daemon, and filtering by it there would ask about
    /// nothing and lose every receipt. Only a TRACKED SEND can have opens, so an
    /// untracked or inbound id answers with an empty list and costs one entry in
    /// a request that was being made anyway.
    /// The whole pass is skipped while the daemon has no tracking configured —
    /// then there are no receipts anywhere and this would be pure round-trips.
    private func refreshOpens() async {
        guard store.trackingAvailable, let messages = thread?.messages, !messages.isEmpty
        else {
            opens = [:]
            return
        }
        opens = await ReadReceipts.opens(for: messages.map(\.id))
    }

    /// Best-effort: a failed lookup just leaves the hint in its default state.
    private func refreshUnsub() async {
        guard let newestSender else {
            unsub = nil
            return
        }
        guard let rows = try? await APIClient.shared.getUnsubscribes() else { return }
        unsub = rows.first { $0.sender == newestSender }
    }

    /// Confirmed unsubscribe. 200 -> open the url, then LEAVE: the server
    /// resolved this SENDER's open mail alongside the request (unsubscribing is
    /// a verdict on them, not on one thread), so the reader departs the way e/d
    /// does — an email that stays on screen after being dealt with reads as the
    /// action not having taken. 422 -> swap the card to the "no link — block
    /// instead?" fallback.
    private func runUnsubscribe() async {
        guard let newest, !confirmBusy else { return }
        confirmBusy = true
        defer { confirmBusy = false }
        do {
            let result = try await APIClient.shared.unsubscribe(messageId: newest.id)
            Opener.open(result.url)
            // Drop the resolved rows NOW rather than one poll later.
            store.noteSenderResolved(result.sender)
            store.pushToast("opened unsubscribe page — \(result.sender)", .success)
            confirmMode = nil
            await departAndClose()
        } catch let apiError as APIError where apiError.status == 422 {
            // No http(s) unsubscribe link — offer to block the sender instead.
            confirmMode = .noLink
        } catch {
            store.pushToast(errText(error, "unsubscribe failed"), .error)
            confirmMode = nil
        }
    }

    /// No-link fallback: block the EXACT sender — and leave, for the same
    /// reason unsubscribe does: the rule's sweep resolved this sender's mail.
    private func runBlock() async {
        guard let newestSender, !confirmBusy else { return }
        confirmBusy = true
        defer { confirmBusy = false }
        do {
            // The exact-address rule lets the server resolve this sender's
            // open mail alongside it; drop those rows optimistically to match.
            try await Actions.createBlockRule(sender: newestSender, sourceMessageId: newest?.id)
            store.noteSenderResolved(newestSender)
            store.pushToast("blocked \(newestSender)", .success)
            confirmMode = nil
            await departAndClose()
        } catch {
            store.pushToast(errText(error, "block failed"), .error)
            confirmMode = nil
        }
    }

    /// The LEAVING half of done+next, for the actions whose server call already
    /// resolved the mail (unsubscribe, block): unpin, lift out through the top,
    /// close. No advance — a verdict on a sender is a stopping point, not a
    /// step to the next email; anything else of theirs in the queue is done too.
    private func departAndClose() async {
        let liftedAt = liftOff()
        if let newest { await ImageStore.shared.release(messageId: newest.id) }
        await flightOut(since: liftedAt)
        // Esc can close the reader while the email is still leaving; a second
        // close must not fire against whatever surface took its place.
        guard store.threadId == threadId else { return }
        store.closeThread()
    }

    private func retriageThis() async {
        guard let newest, !retriaging else { return }
        retriaging = true
        defer { retriaging = false }
        do {
            let result = try await APIClient.shared.retriage(.message(newest.id))
            store.pushToast(
                result.reset > 0 ? "re-triaging this email…" : "nothing to re-triage here", .info)
        } catch {
            store.pushToast(errText(error, "re-triage failed"), .error)
        }
    }

    private func openDebug() async {
        guard let newest else { return }
        do {
            debugInfo = try await APIClient.shared.getTriageDebug(newest.id)
        } catch {
            store.pushToast(errText(error, "debug fetch failed"), .error)
        }
    }
}

// MARK: - backdrop

/// The reader's ground, in ONE place: RootView paints the same backdrop over
/// the title strip above the rail while a thread is open — the strip the
/// reader's inset leaves uncovered — and the two must never drift, or the top
/// bar seams at the rail's edge again.
struct ReaderBackdrop: View {
    var body: some View {
        Rectangle()
            .fill(Palette.readerBackground.opacity(0.97))
            .background(.regularMaterial)
    }
}

// MARK: - message card

/// ONE container per message, and it is the web frame's own rounded clip.
///
/// No fill, no border, no shadow: those are nested shapes carrying zero
/// information around a frame that already clips itself round, paid on every
/// message in a surface whose whole job is reading. Messages are divided by a
/// hairline and marked by a rule, and the mail is the only thing with edges.
///
/// THE CHAT STYLE CHANGES THE ARRANGEMENT AND NOTHING ELSE. The same avatar,
/// the same attention chip, the same read receipt, the same web frame and the
/// same quoted-history collapse — moved into a caption over a bubble on the
/// sender's own side. The rails stay where they are in both, because they are
/// what j/k moves and a navigation affordance that jumps sides is no affordance.
private struct MessageCard: View, Equatable {
    // NONISOLATED, all six: a View is main-actor isolated, and Equatable's
    // requirement is not, so the comparison below can only reach fields that
    // have stepped outside the actor. Every one of them is a value type the
    // wire already declares Sendable, so stepping out costs nothing. The
    // callback stays isolated, which is exactly why `==` cannot see it.
    nonisolated let message: ClientMessage
    /// Cards or bubbles. Part of the comparison because it changes the whole
    /// arrangement AND the measure the body is laid out at.
    nonisolated let style: ThreadStyle
    /// Display index. Carried only so `==` can see it: the callback below
    /// captures it, so two cards that agree about everything else and disagree
    /// about this one would select the wrong message if the older callback were
    /// kept.
    nonisolated let position: Int
    nonisolated let selected: Bool
    /// The first message needs no divider above it: that is the top of the
    /// document, not a seam between two messages.
    nonisolated let ruled: Bool
    /// Recorded opens of this message, when it is one of the user's own tracked
    /// sends. Empty for everything else, which renders no mark.
    nonisolated let opens: [MessageOpen]
    let onSelect: () -> Void

    /// The gutter the rules live in, which every body is inset by. Named
    /// because the measuring pass has to render at exactly the width the mail
    /// will be laid out at, and it derives that from the column minus this.
    static let bodyInset: CGFloat = 13

    /// EVERYTHING BUT THE CALLBACK, which is what makes the card diffable at
    /// all — see the `.equatable()` at the call site. Rebuilding a card means
    /// rebuilding its sandboxed web frame's view value and running that frame's
    /// update, so a card that has not changed must be able to say so.
    nonisolated static func == (a: MessageCard, b: MessageCard) -> Bool {
        a.message == b.message && a.style == b.style && a.position == b.position
            && a.selected == b.selected && a.ruled == b.ruled && a.opens == b.opens
    }

    /// Which side of the conversation this is. Only the chat style asks.
    private var mine: Bool { message.fromMe }
    private var side: HorizontalAlignment { mine ? .trailing : .leading }
    private var edge: Alignment { mine ? .trailing : .leading }
    private var chat: Bool { style == .bubbles }

    var body: some View {
        // ONE TREE FOR BOTH STYLES, and it is load-bearing: a `switch style`
        // here would make the two styles separate view identities, and a flip
        // would tear down every message's subtree — the web frame's remote-image
        // grant, the expanded quoted history, the measured heights, all of it
        // @State that only survives while its structural position does. So the
        // style is expressed entirely in VALUES (alignments, paddings, widths)
        // on one stack whose stateful children never move. The header/caption
        // swap is the one conditional, and nothing in either holds state worth
        // keeping.
        VStack(alignment: chat ? side : .leading, spacing: chat ? 5 : 9) {
            if chat { caption } else { cardHeader }
            mail
            AttachmentStrip(attachments: message.attachmentList, inBody: inBodyImages)
        }
        .frame(
            maxWidth: chat ? ThreadViewer.bubbleWidth : .infinity,
            alignment: chat ? edge : .leading
        )
        .frame(maxWidth: .infinity, alignment: chat ? edge : .leading)
        // The gutter is reserved whether or not this message is selected, so
        // j/k moves a rule rather than shifting every body left and right. The
        // chat style reserves the far side too, or a sent bubble would sit
        // against an edge its neighbours keep clear of.
        .padding(.leading, Self.bodyInset)
        .padding(.trailing, style == .bubbles ? Self.bodyInset : 0)
        // The divider is what separates two cards; bubbles are separated by the
        // air between them, so that is what the padding buys there.
        .padding(.vertical, style == .bubbles ? 5 : 16)
        .frame(maxWidth: .infinity, alignment: .leading)
        // BOTH rules stay mounted and only change opacity. A conditional
        // modifier here would give selected and unselected separate view
        // identities, so every j/k would tear down the message's subtree and
        // make its web frame re-measure from scratch.
        //
        // The attention rail shares the selection rail's slot and yields to it:
        // selection is where you ARE, and two adjacent rails would read as a
        // rendering glitch. The chip in the header keeps marking the message
        // while it is selected.
        .overlay(alignment: .leading) {
            RoundedRectangle(cornerRadius: 1.5, style: .continuous)
                .fill(message.tier == .pastDue ? Palette.danger : Palette.warn)
                .frame(width: 3)
                .padding(.vertical, railInset)
                .opacity(message.needsAttention && !selected ? 0.75 : 0)
        }
        .overlay(alignment: .leading) {
            RoundedRectangle(cornerRadius: 1.5, style: .continuous)
                .fill(Palette.accent)
                .frame(width: 3)
                .padding(.vertical, railInset)
                .opacity(selected ? 1 : 0)
        }
        // ONLY THE CARDS ARE RULED. A hairline between two bubbles would draw a
        // seam across the gap that is already saying the same thing.
        .overlay(alignment: .top) {
            Hairline().opacity(ruled && style == .classic ? 1 : 0)
        }
        .contentShape(Rectangle())
        .onTapGesture(perform: onSelect)
    }

    /// HOW TALL THIS BODY WILL BE, before anything has rendered it — the size
    /// the web frame is given while it is still loading, so the message that
    /// finishes measuring itself mid-scroll corrects its own height by a little
    /// instead of shoving the rest of the thread down the window.
    ///
    /// Off the FLATTENED TEXT with the quoted chain taken off it, because that
    /// is what the frame will actually draw: the injected script collapses
    /// trailing quoted history before its first measurement, so a one-line
    /// reply that quotes a hundred-message thread renders as one line. Guessing
    /// from the whole body would be as wrong as the flat placeholder was, just
    /// in the other direction.
    ///
    /// Memoized under the SAME key the heights are — style and all, because a
    /// bubble fits fewer characters on a line than a card does, so the guess is
    /// a different number for the same words. Read from a view body, and the
    /// split is a regex walk of every line of the message.
    static func guessedBody(_ message: ClientMessage, style: ThreadStyle) -> CGFloat {
        FrameHeights.shared.guess(style.frameKey(message.id)) {
            MinimapGeometry.textHeight(
                text: Quotes.splitText(message.content).visible, html: message.html, style: style)
        }
    }

    /// A short message is a short bubble, so the rail cannot be inset as far as
    /// a card's or there would be nothing left of it to see.
    private var railInset: CGFloat { style == .bubbles ? 6 : 11 }

    // MARK: - the email card's header

    private var cardHeader: some View {
        HStack(spacing: 9) {
            Avatar(sender: message.senderString, size: 24)
            Text(SenderCache.resolved(message.senderString).displayName)
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(Palette.ink)
            Spacer(minLength: 8)
            attentionChip
            ReadReceiptMark(opens: opens)
            Text(Fmt.dateTime(message.received_at))
                .font(Typo.num(11))
                .foregroundStyle(Palette.inkFaintest)
        }
    }

    // MARK: - the chat bubble's caption

    /// Who and when, small, over the bubble on the sender's own side. The avatar
    /// is the received side's alone — a face beside every one of your own lines
    /// is a face you already know. The read receipt is NOT gated on the side:
    /// the card renders it unconditionally, `is_sent` is absent on an older
    /// daemon (which draws everything as theirs), and the mark already no-ops on
    /// empty opens — a gate would only hide receipts the card shows.
    private var caption: some View {
        HStack(spacing: 6) {
            if !mine {
                Avatar(sender: message.senderString, size: 17)
            }
            Text(SenderCache.resolved(message.senderString).displayName)
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(Palette.inkDim)
                .lineLimit(1)
            attentionChip
            ReadReceiptMark(opens: opens)
            Text(captionTime)
                .font(Typo.num(10))
                .foregroundStyle(Palette.inkFaintest)
        }
    }

    /// A caption has room for a clock and not for a calendar, so today's mail
    /// says the time and everything older keeps the date it needs.
    private var captionTime: String {
        Fmt.isToday(message.received_at)
            ? Fmt.timeOfDay(message.received_at) : Fmt.dateTime(message.received_at)
    }

    // MARK: - shared parts

    /// THE ATTENTION MARK: this message's own unresolved standing-tier verdict —
    /// the reason the thread surfaced. Same chip grammar as the list rows, so
    /// the mark reads as "that row, this message".
    @ViewBuilder
    private var attentionChip: some View {
        if message.needsAttention {
            let chip = Fmt.deadlineChip(message.deadline)
            Chip(
                text: chip?.text ?? "needs attention",
                tone: (chip?.overdue ?? false) ? Palette.danger : Palette.warn,
                filled: chip?.overdue ?? false
            )
            .help(message.one_line ?? "this message put the thread in for-your-eyes")
        }
    }

    /// The mail itself. HTML brings its own bubble: the frame's document is an
    /// opaque white page with rounded corners already, so the chat style only
    /// has to hand it a narrower measure. Plain text gets the fill, tinted on
    /// the user's own side.
    ///
    /// The frame key carries the style (see ThreadStyle.frameKey) because a
    /// document measured at 620 points is not the one a full-width card wants
    /// back out of the pool.
    ///
    /// ONE PlainBody FOR BOTH STYLES, for the same identity reason as `body`:
    /// a card branch and a bubble branch would be two positions in the same
    /// conditional, and a flip would collapse the quoted history the reader
    /// opened. The bubble's chrome is values on that one view — zero padding
    /// and a clear fill ARE the card.
    @ViewBuilder
    private var mail: some View {
        if let html = message.html, !html.isEmpty {
            EmailWebView(
                html: html, cacheKey: style.frameKey(message.id),
                allowTrackers: message.allowsTrackers,
                attachments: message.attachmentList,
                textHeight: Self.guessedBody(message, style: style))
                // The web frame's document is hardcoded #fff (EmailFrame), so
                // white IS this bubble's fill, in either theme.
                .overlay(alignment: mine ? .bottomTrailing : .bottomLeading) {
                    tail(Color.white)
                }
        } else {
            PlainBody(content: message.content, fills: !chat)
                .padding(.horizontal, chat ? 12 : 0)
                .padding(.vertical, chat ? 9 : 0)
                // The same corner the web frame clips itself to, so a plain
                // reply and an html one are the same shape on the same side.
                .background(
                    RoundedRectangle(cornerRadius: 10, style: .continuous)
                        .fill(
                            !chat
                                ? Color.clear
                                : mine ? Palette.accentSoft : Palette.hairline)
                )
                .overlay(alignment: mine ? .bottomTrailing : .bottomLeading) {
                    tail(mine ? Palette.accentSoft : Palette.hairline)
                }
        }
    }

    /// The speech tail, hung under the bubble's OUTER bottom corner and poking
    /// past it — the message's side said twice, once by alignment and once by
    /// the point. Mounted in both styles and merely clear in classic, for the
    /// same identity reason as everything else on this card.
    private func tail(_ color: Color) -> some View {
        BubbleTail(mine: mine)
            .fill(chat ? color : Color.clear)
            .frame(width: 12, height: 12)
            .offset(x: mine ? BubbleTail.poke : -BubbleTail.poke)
            .allowsHitTesting(false)
    }

    /// The parts the BODY already shows, because a `cid:` reference in the html
    /// resolved to them — the strip must not paste those a second time under the
    /// message. Recomputed with the card rather than remembered: the answer is a
    /// substring probe for bodies with no `cid:` in them at all, which is nearly
    /// all of them, and the alternative is a second cache to keep in step with
    /// the prepared one.
    ///
    /// It has to read the SAME body the rewrite reads, which is the
    /// tracker-stripped one under this message's own policy — see
    /// EmailWebView.Prepared.make. The hidden and 1×1 tests there are blind to
    /// the scheme, so a `cid:` image the sender hid never reaches the rewrite,
    /// and counting it here would take the tile away from a photo nothing draws.
    /// Only a body that actually names a cid pays for the extra pass.
    private var inBodyImages: Set<Int> {
        guard let html = message.html, !html.isEmpty,
            html.range(of: "cid:", options: .caseInsensitive) != nil
        else { return [] }
        let body = message.allowsTrackers ? html : Trackers.strip(html).html
        return CidImages.referencedAttachmentIDs(html: body, attachments: message.attachmentList)
    }
}

/// The little point under a bubble's OUTER bottom corner — the part that makes
/// a rounded rectangle read as speech. Drawn for the trailing (sent) side and
/// mirrored for the leading one.
///
/// The box overlaps the bubble by its inner span and is FILLED SOLID up to its
/// top edge there, which is what buries the bubble's corner radius: the arc
/// carves a wedge out of the corner, and a tail whose top boundary dips below
/// the arc leaves that wedge showing through as a notch. The outer curve then
/// starts ON the bubble's own edge line, heading down before it bends out, so
/// the silhouette runs straight off the bubble's side into the point.
private struct BubbleTail: Shape {
    /// How far the point pokes past the bubble's edge; the view's offset uses
    /// the same number so the shape's idea of "the edge" is where the edge is.
    static let poke: CGFloat = 7

    var mine: Bool

    func path(in rect: CGRect) -> Path {
        let edge = rect.maxX - Self.poke
        var p = Path()
        p.move(to: CGPoint(x: 0, y: 0))
        p.addLine(to: CGPoint(x: edge, y: 0))
        // Leaves the edge heading DOWN (control barely outboard), then bends
        // out to the point — the vertical tangent is what makes the bubble's
        // side and the tail read as one line.
        p.addQuadCurve(
            to: CGPoint(x: rect.maxX, y: rect.maxY),
            control: CGPoint(x: edge + (rect.maxX - edge) * 0.3, y: rect.maxY * 0.65))
        p.addQuadCurve(
            to: CGPoint(x: 0, y: rect.maxY),
            control: CGPoint(x: edge * 0.8, y: rect.maxY))
        p.closeSubpath()
        guard !mine else { return p }
        return p.applying(
            CGAffineTransform(scaleX: -1, y: 1).translatedBy(x: -rect.width, y: 0))
    }
}

/// The style switch, riding the mail's own top-right corner: two icon slots —
/// text above, talk below — and one highlight that travels between them. A
/// radio rather than a flip because two icons can SAY both answers where the
/// old header button could only name the other one. Same crossing time as
/// GlassSegmented, so every selector in the app moves at one speed.
private struct StyleRadio: View {
    let style: ThreadStyle
    let ready: Bool
    let choose: (ThreadStyle) -> Void

    private static let slot: CGFloat = 26
    private static let gap: CGFloat = 2
    private static let pad: CGFloat = 3

    var body: some View {
        VStack(spacing: Self.gap) {
            option(.classic, icon: "text.alignleft", help: "read this thread as email cards")
            option(.bubbles, icon: "bubble.left", help: "read this thread as chat bubbles")
        }
        .padding(Self.pad)
        .background(alignment: .top) { highlight }
        .animation(.smooth(duration: 0.32), value: style)
        .background(
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .fill(Palette.hairline.opacity(0.5)))
        // Same gate as `b` (see `chooseStyle`): the pin is permanent, and there
        // is nothing to answer about until the thread on screen is this one.
        .opacity(ready ? 1 : 0.35)
        .disabled(!ready)
    }

    private func option(_ value: ThreadStyle, icon: String, help: String) -> some View {
        Button { choose(value) } label: {
            Image(systemName: icon)
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(style == value ? .white : Palette.inkDim)
                .frame(width: Self.slot, height: Self.slot)
                .contentShape(RoundedRectangle(cornerRadius: 6, style: .continuous))
        }
        .buttonStyle(.plain)
        .help(help)
    }

    /// The travelling pane, one view for the life of the control — the slots
    /// are fixed squares, so the offset is arithmetic rather than measurement.
    private var highlight: some View {
        Color.clear
            .frame(width: Self.slot, height: Self.slot)
            .glassEffect(
                .regular.tint(Palette.accent.opacity(0.55)),
                in: RoundedRectangle(cornerRadius: 6, style: .continuous)
            )
            // Sits over the active slot — an interactive material would eat
            // that option's clicks.
            .allowsHitTesting(false)
            .offset(y: Self.pad + (style == .bubbles ? Self.slot + Self.gap : 0))
    }
}

/// A plain-text body with its trailing quoted history collapsed behind a chip.
/// Mirrors the html-side collapse; the split heuristic is shared (Quotes) and
/// conservative — when in doubt the full text renders.
///
/// Internal rather than private because the FORWARD COMPOSER renders the message
/// it is passing on with the reader's own pair of body views (this and
/// `EmailWebView`), chosen by the reader's own test. A second plain-text
/// renderer over there would be a second set of collapse rules, drifting from
/// the one the reader spent this file getting right.
struct PlainBody: View {
    let content: String
    /// Whether the text takes the whole measure it is offered. A card's body IS
    /// the column, so it does; a bubble is only as wide as what is in it, and a
    /// flexible frame with no bound is the child's own size.
    var fills = true
    @State private var open = false

    var body: some View {
        let split = Quotes.splitText(content)
        VStack(alignment: .leading, spacing: 8) {
            Text(split.quoted == nil ? content : split.visible)
                .font(.system(size: 13.5))
                .foregroundStyle(Palette.ink)
                .textSelection(.enabled)
                .frame(maxWidth: fills ? .infinity : nil, alignment: .leading)
                .fixedSize(horizontal: false, vertical: true)
            if let quoted = split.quoted {
                Button {
                    open.toggle()
                } label: {
                    Text(open ? "hide quoted history" : "··· show quoted history")
                        .font(Typo.micro)
                        .padding(.horizontal, 9).padding(.vertical, 3)
                }
                // Same text-on-hover treatment as the header actions: this is
                // the only control INSIDE the reading surface, so a glass pill
                // here read as a second piece of chrome stapled to the mail.
                .buttonStyle(.textAction)
                .help("the quoted reply chain below this message")
                if open {
                    Text(quoted)
                        .font(.system(size: 13))
                        .foregroundStyle(Palette.inkFaint)
                        .textSelection(.enabled)
                        .padding(.leading, 10)
                        .overlay(alignment: .leading) {
                            Rectangle().fill(Palette.hairline).frame(width: 2)
                        }
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }
    }
}

// MARK: - unsubscribe confirm

/// Confirm-first unsubscribe. Pushes the "modal" KeyContext (Enter confirms,
/// Esc cancels) so the thread's j/k/e/d/u never fire while it's open. `mode`
/// swaps the copy between the initial confirm and the 422 fallback.
private struct UnsubConfirm: View {
    let mode: ThreadViewer.ConfirmMode
    let senderName: String
    let busy: Bool
    let onConfirm: () -> Void
    let onCancel: () -> Void

    var body: some View {
        OverlayScrim(onDismiss: onCancel) {
            ModalCard(width: 400) {
                if mode == .ask {
                    Text("Unsubscribe from **\(senderName)**?")
                        .font(.system(size: 14))
                        .foregroundStyle(Palette.ink)
                } else {
                    Text("No unsubscribe link on this email. Block **\(senderName)** instead?")
                        .font(.system(size: 14))
                        .foregroundStyle(Palette.ink)
                        .fixedSize(horizontal: false, vertical: true)
                }
                HStack(spacing: 8) {
                    Spacer()
                    Button("Cancel", action: onCancel)
                        .buttonStyle(.glass).disabled(busy)
                    Button(mode == .ask ? "Unsubscribe" : "Block sender", action: onConfirm)
                        .buttonStyle(.glassProminent)
                        .tint(mode == .ask ? Palette.accent : Palette.danger)
                        .disabled(busy)
                }
            }
        }
        .keyContext(.modal)
        .keyBindings(.modal, [
            KeyBinding("Escape", "cancel", allowInInput: true) { onCancel() },
            KeyBinding("Enter", "confirm", allowInInput: true) { onConfirm() },
        ])
    }
}

// MARK: - dev triage inspector

/// DEV overlay: the full triage row as key/value mono rows. Own "modal"
/// KeyContext so Esc closes it without leaking to the thread keys underneath.
private struct TriageDebugOverlay: View {
    let info: TriageDebug
    let onClose: () -> Void

    private var rows: [(String, String)] {
        [
            ("message_id", String(info.message_id)),
            ("subject", info.subject),
            ("importance", String(info.importance)),
            ("tier", info.tier),
            ("category", info.category ?? "null"),
            ("one_line", info.one_line),
            ("reason", info.reason),
            ("reason.importance", info.field_reasons?.importance ?? "null"),
            ("reason.deadline", info.field_reasons?.deadline ?? "null"),
            ("reason.tier", info.field_reasons?.tier ?? "null"),
            ("deadline", info.deadline ?? "null"),
            ("matched_rule_id", info.matched_rule_id.map(String.init) ?? "null"),
            ("status", info.status),
            ("surfaced_at", info.surfaced_at ?? "null"),
            ("resolved_at", info.resolved_at ?? "null"),
            ("stage1_model_used", info.stage1_model_used ?? "null"),
            ("model_used (stage2)", info.model_used ?? "null"),
            ("needs_stage2", String(info.needs_stage2)),
            ("extractor_model_used", info.extractor_model_used ?? "null"),
            ("created_at", info.created_at),
        ]
    }

    var body: some View {
        OverlayScrim(onDismiss: onClose) {
            ModalCard(width: 560) {
                HStack {
                    Text("triage debug")
                        .font(Typo.sectionLabel)
                        .foregroundStyle(Palette.inkFaint)
                        .textCase(.uppercase)
                    Spacer()
                    HStack(spacing: 4) {
                        Kbd("esc")
                        Text("close").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                    }
                }
                ScrollView {
                    VStack(alignment: .leading, spacing: 3) {
                        ForEach(rows, id: \.0) { key, value in
                            HStack(alignment: .top, spacing: 10) {
                                Text(key)
                                    .font(Typo.mono(10))
                                    .foregroundStyle(Palette.inkFaintest)
                                    .frame(width: 150, alignment: .leading)
                                Text(value)
                                    .font(Typo.mono(10))
                                    .foregroundStyle(Palette.inkDim)
                                    .textSelection(.enabled)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                            }
                        }
                    }
                }
                .frame(maxHeight: 420)
            }
        }
        .keyContext(.modal)
        .keyBindings(.modal, [
            KeyBinding("Escape", "close", allowInInput: true) { onClose() }
        ])
    }
}

/// A settle loop's watch for the reader's own wheel, macOS only: the loop
/// re-aims for up to a second after a style flip, and a wheel scroll never
/// moves the selection its guard is watching, so without this the loop drags
/// the reader back to its target for the rest of the leash. A LOCAL monitor
/// sees the event before the scroll view does, trips the flag, and passes the
/// event through untouched.
///
/// Owned by ONE loop and ended by it on every exit path — a leaked monitor
/// watches every scroll the app ever makes, and a SHARED one would let a loop
/// ending late tear down its successor's watch. On iOS the flip is a header
/// tap and the leash is a second; there is no monitor to install and the flag
/// simply never trips.
@MainActor
private final class WheelYield {
    private(set) var tripped = false
    private var monitor: Any?

    func watch() {
        #if os(macOS)
            monitor = NSEvent.addLocalMonitorForEvents(matching: .scrollWheel) {
                [weak self] event in
                MainActor.assumeIsolated { self?.tripped = true }
                return event
            }
        #endif
    }

    func end() {
        #if os(macOS)
            if let monitor { NSEvent.removeMonitor(monitor) }
        #endif
        monitor = nil
    }
}
