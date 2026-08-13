// FULLSCREEN THREAD VIEWER. Fetches GET /client/thread/{id} and stacks every
// message NEWEST-FIRST; j/k move the selection (j = older). HTML renders in the
// hard-sandboxed EmailWebView, plain text in a selectable card. The outer column
// is the ONE scroll surface — message frames size to their content and never
// scroll internally. Esc, or a gutter click, closes back onto the surface below.
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
    /// messageId -> image srcs an earlier message in this thread already showed.
    /// Rebuilt with the thread and thrown away with it; nothing crosses threads.
    @State private var repeatedImages: [Int: Set<String>] = [:]
    /// messageId -> recorded opens of the user's own tracked sends. Only sent,
    /// tracked messages ever have an entry; see `refreshOpens`.
    @State private var opens: [Int: [MessageOpen]] = [:]
    /// Display indices of the rows on screen right now. The wheel moves what is
    /// visible without touching the selection, and `refreshInPlace` must not
    /// treat a wheel reader as "at the newest" just because they never pressed
    /// `j` — see `anchorId`.
    @State private var visibleIndices: Set<Int> = []

    enum ConfirmMode: Equatable { case ask, noLink }

    /// Server order is chronological; display order is newest-first.
    private var messages: [ClientMessage] { (thread?.messages ?? []).reversed() }
    /// The NEWEST message is what `u` acts on (the server derives the sender
    /// from it) and whose from_addr keys the record lookup.
    private var newest: ClientMessage? { messages.first }
    /// trim().lowercased() mirrors the server's canonical `sender`.
    private var newestSender: String? {
        newest.map { $0.from_addr.trimmingCharacters(in: .whitespaces).lowercased() }
    }
    private var senderName: String { newest.map { SenderCache.resolved($0.senderString).displayName } ?? "" }

    /// EVERY participant, in first-appearance order, deduped by canonical address.
    /// Reads `thread.messages` (chronological), not the newest-first display order,
    /// so the list starts with whoever started the thread.
    private var participants: [String] {
        var seen = Set<String>()
        var names: [String] = []
        for m in thread?.messages ?? [] {
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

    /// Display index of the first message needing attention (newest-first order,
    /// so ties go to the most recent obligation), or nil for a calm thread.
    private var attentionIndex: Int? {
        messages.firstIndex(where: \.needsAttention)
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
                        Image(systemName: "arrow.down.to.line.compact")
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
                ScrollView {
                    // spacing 0: each message owns its vertical rhythm, so the
                    // hairline between two lands mid-gap instead of hugging one.
                    LazyVStack(alignment: .leading, spacing: 0) {
                        ForEach(Array(messages.enumerated()), id: \.element.id) { i, m in
                            MessageCard(
                                message: m, selected: i == index, ruled: i > 0,
                                seenEarlier: repeatedImages[m.id] ?? [],
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
                        }
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
                .onChange(of: index) { _, i in
                    withAnimation(.easeOut(duration: 0.14)) { proxy.scrollTo(i, anchor: .top) }
                }
                // A refetch that PREPENDED a message — a sent reply's own echo —
                // has to land on it, and the watcher above cannot do that job:
                // `adopt` sets index to 0, which it already was, so it sees no
                // change. Unanimated because on the initial load and on a thread
                // switch this is the starting position, not a movement.
                //
                // It scrolls to the SELECTION, not to zero, because the two are
                // no longer the same thing: `refreshInPlace` puts the selection
                // back on the message that was on screen, and a hardcoded 0 here
                // would fire on the same update and undo exactly the position it
                // just preserved. Every other path into this watcher is sitting
                // on 0 already.
                .onChange(of: messages.first?.id) { _, _ in
                    proxy.scrollTo(index, anchor: .top)
                }
            }
        }
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
            KeyBinding("h", "prev email") { stepQueue(-1) },
            KeyBinding("l", "next email") { stepQueue(1) },
            KeyBinding("ArrowLeft", "prev email") { stepQueue(-1) },
            KeyBinding("ArrowRight", "next email") { stepQueue(1) },
            KeyBinding("j", "older message") { index = min(messages.count - 1, index + 1) },
            KeyBinding("k", "newer message") { index = max(0, index - 1) },
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
        // `adopt` read the prefetch's repeated-image map, but the warmer that
        // recomputes it is detached and has not landed — the arrivals have no
        // entry, and their repeated logos would all render again. Derive it
        // from the view in hand instead.
        repeatedImages = ThreadPrefetch.repeatedImages(in: view)
        if let anchor, let found = messages.firstIndex(where: { $0.id == anchor }) {
            index = found
        }
        await refreshOpens()
    }

    /// The message the reader is actually ON, whether they got there with the
    /// keys or the wheel. A moved selection wins; failing that, the topmost
    /// VISIBLE row stands in, because a wheel scroll moves what is on screen
    /// without ever touching the selection — preserving the selection alone
    /// would let a background refresh yank a wheel reader back to the top. nil
    /// means the reader really is sitting on the newest, which is the one case
    /// a refresh may move them: onto the reply they are waiting for.
    private var anchorId: Int? {
        if index != 0 { return messages[safe: index]?.id }
        guard let top = visibleIndices.min(), top > 0 else { return nil }
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
    private func doneAndNext() async {
        let queue = store.threadQueue
        guard let cur = queue.firstIndex(where: { $0.thread_id == threadId }) else {
            // Not opened from a queue (search, a right-rail record): `e` still
            // means done — resolve the newest message directly and close.
            if let newestChrono = thread?.messages.last {
                do {
                    try await APIClient.shared.setStatus(newestChrono.id, .done)
                    // Same unpin as Actions.done — this path resolves the
                    // message without going through it.
                    await ImageStore.shared.release(messageId: newestChrono.id)
                    // And the same optimistic drop: the surface underneath is
                    // still mounted, so without this the reader closes back
                    // onto a row for mail that is already done.
                    store.noteResolved(newestChrono.id)
                    store.pushToast("done", .info)
                } catch {
                    store.pushToast(errText(error, "done failed"), .error)
                }
            }
            store.closeThread()
            return
        }
        await Actions.done(queue[cur])
        if let next = queue[safe: cur + 1] {
            store.openThread(next.thread_id, queue: queue)
        } else {
            store.closeThread()
        }
    }

    // MARK: - data

    private func load() async {
        // A fresh thread mounts fresh rows; what was visible of the LAST one
        // must not anchor this one.
        visibleIndices.removeAll()
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

    /// Take a loaded thread and derive everything per-thread from it ONCE.
    ///
    /// The repeated-image pass in particular must not be a computed property:
    /// it strips and scans every message body, and the surrounding view
    /// re-evaluates on every scroll frame — the same trap EmailWebView's
    /// `Prepared` cache exists to close. The prefetch warmer normally has it
    /// computed already (off the main actor, alongside the prepared bodies);
    /// only a thread that opened ahead of its warmer pays for it here.
    private func adopt(_ view: ClientThreadView) {
        thread = view
        repeatedImages =
            ThreadPrefetch.shared.cachedRepeatedImages(threadId)
            ?? ThreadPrefetch.repeatedImages(in: view)
        index = 0  // newest renders first — land on it
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
        guard let newestChrono = thread?.messages.last, !retriaging else { return }
        retriaging = true
        defer { retriaging = false }
        do {
            let result = try await APIClient.shared.retriage(.message(newestChrono.id))
            store.pushToast(
                result.reset > 0 ? "re-triaging this email…" : "nothing to re-triage here", .info)
        } catch {
            store.pushToast(errText(error, "re-triage failed"), .error)
        }
    }

    private func openDebug() async {
        guard let newestChrono = thread?.messages.last else { return }
        do {
            debugInfo = try await APIClient.shared.getTriageDebug(newestChrono.id)
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
    /// Image srcs an earlier message already showed; dropped from this body.
    let seenEarlier: Set<String>
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
                    html: html, cacheKey: String(message.id), seenEarlier: seenEarlier,
                    allowTrackers: message.allowsTrackers)
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
