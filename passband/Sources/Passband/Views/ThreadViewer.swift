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

    @State private var thread: ClientThreadView?
    @State private var error: String?
    @State private var loading = true
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
    /// Display indices of the rows on screen right now. The wheel moves what is
    /// visible without touching the selection, and `refreshInPlace` must not
    /// treat a wheel reader as "at the newest" just because they never pressed
    /// `j` — see `anchorId`.
    @State private var visibleIndices: Set<Int> = []
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
    /// Whether the opening landing has been taken for this thread. Until it has,
    /// the reader is wherever the initial anchor dropped it, which is not a
    /// position anybody chose.
    @State private var landed = false

    enum ConfirmMode: Equatable { case ask, noLink }

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
            Rectangle()
                .fill(Palette.readerBackground.opacity(0.97))
                .background(.regularMaterial)
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

    /// A Mac reads the whole header on ONE line: subject left, actions right,
    /// the subject taking whatever width the actions leave. A phone has no such
    /// width — four text buttons beside a subject would squeeze it to an
    /// ellipsis — so the same pieces stack, the actions under the line they act
    /// on, and leaving belongs to the navigation bar above rather than to a
    /// button of our own.
    private var header: some View {
        #if os(macOS)
            HStack(alignment: .firstTextBaseline, spacing: 14) {
                titleBlock
                actions
                Button { store.closeThread() } label: {
                    HStack(spacing: 4) {
                        Kbd("esc")
                        Text("back").font(Typo.micro)
                    }
                }
                .buttonStyle(.textAction)
            }
            .padding(.horizontal, 22)
            // PADDING, not the shared `TopBar.height`, and the one header in the
            // app that keeps its own: a subject can wrap to two lines and carries
            // a participant line under it, so a fixed bar height would either
            // squash it or hold empty air for the threads that don't wrap. The
            // subject still lands on the window buttons' line, which is what the
            // bar was for.
            .padding(.vertical, 13)
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

    private var titleBlock: some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                // The subject IS the copy affordance — no icon earns a
                // place in this header for a once-in-a-while verb.
                Button {
                    if let subject = thread?.subject, !subject.isEmpty {
                        Clip.copy(subject, flashing: $subjectCopied)
                    }
                } label: {
                    Text(thread?.subject ?? "…")
                        .font(Typo.serif(19, weight: .medium))
                        .foregroundStyle(Palette.ink)
                        .lineLimit(2)
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
            if !participantLine.isEmpty {
                Text(participantLine)
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.inkFaint)
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
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
                            map: map, marks: minimapMarks, selected: index,
                            viewport: viewportHeight)
                    #endif

                    ScrollView {
                        // spacing 0: each message owns its vertical rhythm, so the
                        // hairline between two lands mid-gap instead of hugging one.
                        LazyVStack(alignment: .leading, spacing: 0) {
                            ForEach(Array(messages.enumerated()), id: \.element.id) { i, m in
                                MessageCard(
                                    message: m, selected: i == index, ruled: i > 0,
                                    opens: opens[m.id] ?? []
                                ) {
                                    index = i
                                }
                                .id(i)
                                .onScrollVisibilityChange(threshold: 0.1) { visible in
                                    if visible {
                                        visibleIndices.insert(i)
                                    } else {
                                        visibleIndices.remove(i)
                                    }
                                }
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
                    guard !landed || parkedOnNewest || was.last != now.last else { return }
                    proxy.scrollTo(newestIndex, anchor: .top)
                    landed = true
                }
                .onChange(of: messages.last?.id) { _, _ in
                    proxy.scrollTo(index, anchor: .top)
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
    private func settle(on target: Int, proxy: ScrollViewProxy) {
        proxy.scrollTo(target, anchor: .top)
        Task { @MainActor in
            for _ in 0..<8 {
                try? await Task.sleep(for: .milliseconds(25))
                guard index == target else { return }
                if let frame = map.frames[target], abs(frame.minY) < 2 { return }
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
    private var minimapMarks: [ThreadMinimap.Mark] {
        messages.map { m in
            ThreadMinimap.Mark(
                attention: m.needsAttention,
                tint: Palette.avatarColors(for: m.senderString).fg,
                estimate: mapEstimate(m))
        }
    }

    /// WHAT THIS MESSAGE IS WORTH ON THE RAIL, from its own text and nothing
    /// else. Deliberately NOT the height the web frame measured for it, even
    /// though the reader keeps those (FrameHeights, for painting a reopened
    /// message at its final size): a map that mixes measured cards with guessed
    /// ones redraws itself as the measurements land, which is a rail that crawls
    /// while you read it. A map that is uniformly approximate holds still, and
    /// holding still is the whole job.
    private func mapEstimate(_ m: ClientMessage) -> CGFloat {
        MinimapGeometry.estimate(
            text: m.content, html: m.html, attachments: m.attachmentList.count)
    }

    /// The mail's measure. The dismissible gutter is defined as the complement
    /// of it, so the two can never drift apart — and the inline composer pins
    /// itself to the same measure, so the reply sits under the column it answers.
    static let columnWidth: CGFloat = 900

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
            // The SELECTED message's sender, not the thread's newest: on a
            // back-and-forth the two differ, and the one you are looking at is
            // the one you mean. Search opens as the strip beside the reader, so
            // this does not cost you the email you are reading.
            KeyBinding("f", "search this sender") {
                guard let m = messages[safe: index] else { return }
                store.openSearch(seed: "from:\(m.from_addr)")
            },
            KeyBinding("e", "done + next") { Task { await doneAndNext() } },
            KeyBinding("d", "done + next") { Task { await doneAndNext() } },
            // Plain `h`, the same key as every other surface — it took "prev
            // email"'s key (see the queue block above), because a verb cannot
            // be the app's one shifted letter.
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
    private var anchorId: Int? {
        if index != newestIndex { return messages[safe: index]?.id }
        guard let top = visibleIndices.min(), top < newestIndex else { return nil }
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

    /// e/d — "done + next": mark the current thread's update done (keeping its
    /// 5s undo), then advance to the NEXT queued update in place; if none
    /// remain, close the viewer.
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
        guard let cur = queue.firstIndex(where: { $0.thread_id == threadId }) else {
            // Not opened from a queue (search, a right-rail record): `e` still
            // means done — resolve the newest message directly and close. It
            // still leaves the same way; there is simply nothing behind it.
            let liftedAt = liftOff()
            if let newest {
                do {
                    try await APIClient.shared.setStatus(newest.id, .done)
                    // Same unpin as Actions.done — this path resolves the
                    // message without going through it.
                    await ImageStore.shared.release(messageId: newest.id)
                    // And the same optimistic drop: the surface underneath is
                    // still mounted, so without this the reader closes back
                    // onto a row for mail that is already done.
                    store.noteResolved(newest.id)
                    store.pushToast("done", .info)
                } catch {
                    store.pushToast(errText(error, "done failed"), .error)
                }
            }
            await flightOut(since: liftedAt)
            store.closeThread()
            return
        }

        let next = queue[safe: cur + 1]
        let liftedAt = liftOff()
        await Actions.done(queue[cur])
        await flightOut(since: liftedAt)

        // The reader can be closed while the email is still leaving — Esc, or
        // another surface taking the screen. A done+next that outlived its own
        // reader must not haul the next thread back onto a page nobody is on.
        guard store.threadId == threadId else { return }

        guard let next else {
            // Nothing behind it: the queue is finished, and the reader leaves
            // with the email rather than snapping back to show an empty one.
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
        // A fresh thread mounts fresh rows; what was visible of the LAST one
        // must not anchor this one, and its shape must not be what the minimap
        // draws for a thread this one knows nothing about yet.
        visibleIndices.removeAll()
        map.forget()
        newestHeight = 0
        landed = false
        // Fresh prefetch hit → render it and skip the round-trip entirely (the
        // cache is at most 60s old; e/d/refresh paths repopulate it).
        if let cached = ThreadPrefetch.shared.cached(threadId) {
            adopt(cached)
            error = nil
            loading = false
            return
        }
        loading = true
        error = nil
        do {
            let view = try await APIClient.shared.getThread(threadId)
            ThreadPrefetch.shared.note(threadId, view)  // instant reopen
            adopt(view)
        } catch {
            self.error = errText(error, "thread load failed")
        }
        loading = false
    }

    /// Take a loaded thread and derive its reader state.
    private func adopt(_ view: ClientThreadView) {
        thread = view
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
    /// Every message is asked about rather than only the ones the user sent:
    /// the wire carries no "this copy is mine" flag, only a TRACKED SEND can
    /// have opens, and an untracked or inbound id answers with an empty list.
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

    /// Confirmed unsubscribe. 200 -> open the url + toast + refresh the hint.
    /// 422 -> swap the card to the "no link — block instead?" fallback.
    private func runUnsubscribe() async {
        guard let newest, !confirmBusy else { return }
        confirmBusy = true
        defer { confirmBusy = false }
        do {
            let result = try await APIClient.shared.unsubscribe(messageId: newest.id)
            Opener.open(result.url)
            // The server resolved this SENDER's open mail (unsubscribing is a
            // verdict on them, not on one thread); drop those rows now rather
            // than waiting on the poll.
            store.noteSenderResolved(result.sender)
            store.pushToast("opened unsubscribe page — \(result.sender)", .success)
            await refreshUnsub()
            confirmMode = nil
        } catch let apiError as APIError where apiError.status == 422 {
            // No http(s) unsubscribe link — offer to block the sender instead.
            confirmMode = .noLink
        } catch {
            store.pushToast(errText(error, "unsubscribe failed"), .error)
            confirmMode = nil
        }
    }

    /// No-link fallback: block the EXACT sender.
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
        } catch {
            store.pushToast(errText(error, "block failed"), .error)
        }
        confirmMode = nil
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

// MARK: - message card

/// ONE container per message, and it is the web frame's own rounded clip.
///
/// No fill, no border, no shadow: those are nested shapes carrying zero
/// information around a frame that already clips itself round, paid on every
/// message in a surface whose whole job is reading. Messages are divided by a
/// hairline and marked by a rule, and the mail is the only thing with edges.
private struct MessageCard: View {
    let message: ClientMessage
    let selected: Bool
    /// The first message needs no divider above it: that is the top of the
    /// document, not a seam between two messages.
    let ruled: Bool
    /// Recorded opens of this message, when it is one of the user's own tracked
    /// sends. Empty for everything else, which renders no mark.
    let opens: [MessageOpen]
    let onSelect: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 9) {
            HStack(spacing: 9) {
                Avatar(sender: message.senderString, size: 24)
                Text(SenderCache.resolved(message.senderString).displayName)
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(Palette.ink)
                Spacer(minLength: 8)
                // THE ATTENTION MARK: this message's own unresolved standing-tier
                // verdict — the reason the thread surfaced. Same chip grammar as
                // the list rows, so the mark reads as "that row, this message".
                if message.needsAttention {
                    let chip = Fmt.deadlineChip(message.deadline)
                    Chip(
                        text: chip?.text ?? "needs attention",
                        tone: (chip?.overdue ?? false) ? Palette.danger : Palette.warn,
                        filled: chip?.overdue ?? false
                    )
                    .help(message.one_line ?? "this message put the thread in for-your-eyes")
                }
                ReadReceiptMark(opens: opens)
                Text(Fmt.dateTime(message.received_at))
                    .font(Typo.num(11))
                    .foregroundStyle(Palette.inkFaintest)
            }

            if let html = message.html, !html.isEmpty {
                EmailWebView(
                    html: html, cacheKey: String(message.id), allowTrackers: message.allowsTrackers)
            } else {
                PlainBody(content: message.content)
            }

            AttachmentStrip(attachments: message.attachmentList)
        }
        // The gutter is reserved whether or not this message is selected, so
        // j/k moves a rule rather than shifting every body left and right.
        .padding(.leading, 13)
        .padding(.vertical, 16)
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
                .padding(.vertical, 11)
                .opacity(message.needsAttention && !selected ? 0.75 : 0)
        }
        .overlay(alignment: .leading) {
            RoundedRectangle(cornerRadius: 1.5, style: .continuous)
                .fill(Palette.accent)
                .frame(width: 3)
                .padding(.vertical, 11)
                .opacity(selected ? 1 : 0)
        }
        .overlay(alignment: .top) {
            Hairline().opacity(ruled ? 1 : 0)
        }
        .contentShape(Rectangle())
        .onTapGesture(perform: onSelect)
    }
}

/// A plain-text body with its trailing quoted history collapsed behind a chip.
/// Mirrors the html-side collapse; the split heuristic is shared (Quotes) and
/// conservative — when in doubt the full text renders.
private struct PlainBody: View {
    let content: String
    @State private var open = false

    var body: some View {
        let split = Quotes.splitText(content)
        VStack(alignment: .leading, spacing: 8) {
            Text(split.quoted == nil ? content : split.visible)
                .font(.system(size: 13.5))
                .foregroundStyle(Palette.ink)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
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
