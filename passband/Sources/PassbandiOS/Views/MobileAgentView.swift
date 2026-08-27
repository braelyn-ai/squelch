// THE AGENT, ON A THUMB. Same session, same verbs as the Mac's ⌘K ask bar —
// `store.assistant` is one `AssistantSession` for the whole app, so a run
// started here streams into the same transcript, parks on the same confirm
// cards, and spends the same BYOK key. Only the SHELL differs, and it differs
// the way every other phone surface does: the Mac's 620pt modal that grows a
// tray out of its own bottom edge becomes a whole screen, because a phone has
// no window to float over and nothing to float above.
//
// IT IS PUSHED FROM THE SEARCH FIELD, and it has no tab of its own. The phone
// has one text field per screen, so the field that asks the agent is the field
// that searches; this screen is where those words land. Arriving with an
// `initialQuestion` means the push itself WAS the send — the ask row was
// tapped, or return was pressed on a five-word question — so the run starts on
// appear rather than sitting in the composer waiting to be confirmed twice.
//
// WHAT THAT COSTS AND WHAT IT BUYS. The bar's glass container, its ⌘K/esc
// chords and its "the conversation is behind a modal" framing are gone; back
// lands on the results the question came from, and the transcript survives the
// pop because the session lives on the store. In exchange every row is re-laid
// out for a fingertip: bigger type, real tap targets on the cards, and confirm
// buttons that span the card instead of sitting in a 11pt row.
//
// THE ROW RENDERING IS PORTED, NOT SHARED, and deliberately: Views/AskBar.swift
// is excluded from this target and stays that way. Its rows encode desktop
// sizes and desktop affordances (hover washes, keyboard hints, help tooltips)
// at every level, so a "shared" row would be a pile of `#if os` inside a view
// whose whole job is layout. What IS shared is everything that matters — the
// session, the transcript types, the confirm ceremony, the composer handoff.

import SwiftUI

struct MobileAgentView: View {
    @Environment(AppStore.self) private var store
    /// Read for the transport pref alone: whether an ask rides the daemon's
    /// relay or this phone's own key decides whether there is a key to miss.
    @Environment(Prefs.self) private var prefs

    /// The words that opened this screen, sent the moment it appears. nil when
    /// the chat was walked into rather than asked into.
    var initialQuestion: String?

    @State private var question = ""
    @FocusState private var focused: Bool
    /// Rendered markdown, kept across body evaluations — see MobileMarkdownCache.
    @State private var rendered = MobileMarkdownCache()
    /// nil until the keychain answers. Unknown reads as "fine": the gate below
    /// swaps in for the composer, and flashing an explainer at a user who has a
    /// key set is the worse of the two wrong frames.
    @State private var keyStatus: AssistantKeyStatus?
    /// The opening question has been sent. See `sendInitialQuestion`.
    @State private var asked = false

    /// The conversation outlives this view — see AppStore.assistant.
    private var session: AssistantSession { store.assistant }

    /// Scroll target pinned to the bottom of the log.
    private static let bottomAnchor = "agent.bottom"

    var body: some View {
        transcript
            .background(Palette.canvas)
            .navigationTitle("Agent")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    Button { newConversation() } label: {
                        Image(systemName: "plus.bubble")
                    }
                    .disabled(session.transcript.isEmpty)
                    .accessibilityLabel("New conversation")
                }
            }
            // The composer rides the keyboard: a safe-area inset is the one
            // placement iOS raises with it, so the field never ends up under the
            // keys it is being typed on.
            .safeAreaInset(edge: .bottom) {
                if keyMissing { keyGate } else { composer }
            }
            // STRUCTURE ONLY. Animating anything that changes per token would
            // smear the streaming text; the count changes once per row.
            .animation(.smooth(duration: 0.25), value: session.transcript.count)
            // The key gets pasted in SETTINGS, on another tab, after this screen
            // has already been looked at once. Re-reading on every appearance is
            // what lets walking back in turn the agent on.
            .onAppear {
                // SEQUENCED, not parallel: the auto-send must not race the
                // keychain. Firing before the key status lands would spend the
                // first visit of every keyless install on a provider error row,
                // painted moments before the key gate swaps in beneath it.
                Task {
                    await refreshKeyStatus()
                    sendInitialQuestion()
                }
            }
    }

    // MARK: - the log

    private var transcript: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 14) {
                    if session.transcript.isEmpty {
                        opener
                    }
                    ForEach(session.transcript) { item in
                        row(item).frame(maxWidth: .infinity, alignment: .leading)
                    }
                    if session.awaitingOutput {
                        workingRow
                    } else if parkedOnConfirmation {
                        parkedRow
                    }
                    Color.clear.frame(height: 1).id(Self.bottomAnchor)
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 14)
            }
            // MOUNT AT THE BOTTOM. Both follows below are onChange, and neither
            // fires on appear — so coming back to this tab mid-answer would
            // otherwise restore the conversation scrolled to its beginning, with
            // the card that is holding the run hostage off the bottom.
            .defaultScrollAnchor(.bottom)
            // The anchor alone is applied BEFORE the lazy rows finish sizing, so
            // a long conversation still lands short of the end. Re-pin after
            // layout settles.
            .onAppear {
                Task { @MainActor in
                    proxy.scrollTo(Self.bottomAnchor, anchor: .bottom)
                }
            }
            // A new row is a structural change worth following visibly.
            .onChange(of: session.transcript.count) { _, _ in
                withAnimation(Motion.scrollFollow) {
                    proxy.scrollTo(Self.bottomAnchor, anchor: .bottom)
                }
            }
            // Tokens are not. Following them UNANIMATED is what keeps the text
            // legible while it lands — an animated follow at token rate smears
            // the type.
            .onChange(of: session.streamTick) { _, _ in
                proxy.scrollTo(Self.bottomAnchor, anchor: .bottom)
            }
            // Drag the log down and the keyboard goes with it, so a long answer
            // can be read without dismissing anything first.
            .scrollDismissesKeyboard(.interactively)
            // EVERY LINK IN HERE GOES THROUGH THE HOUSE DOOR. Assistant answers
            // render as markdown and the model's input is attacker-authored mail
            // bodies, so `[Verify your account](…)` is a link somebody else
            // wrote. Opener is the same http(s)-only guard the reader holds
            // (docs/SECURITY.md §2 layer 4).
            .environment(
                \.openURL,
                OpenURLAction { url in
                    Opener.open(url.absoluteString)
                    return .handled
                })
        }
    }

    /// True while the loop is suspended on a card nobody has answered. `running`
    /// stays true through that (the loop really is open), so send is disabled
    /// with nothing on screen to say why.
    private var parkedOnConfirmation: Bool {
        guard session.running, let last = session.transcript.last else { return false }
        return last.action?.state == .pending
    }

    @ViewBuilder
    private func row(_ item: ChatItem) -> some View {
        switch item.kind {
        case .user:
            userRow(item.text)
        case .assistant:
            Text(assistantText(item))
                .font(.system(size: 15))
                .foregroundStyle(Palette.ink)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        case .tool:
            if let tool = item.tool { MobileToolChip(tool: tool) }
        case .action:
            if let action = item.action { actionCard(action) }
        case .error:
            Text(item.text)
                .font(.system(size: 14))
                .foregroundStyle(Palette.danger)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        case .citations:
            citations(item.citations)
        case .emails:
            emailCards(item.emails)
        }
    }

    private func userRow(_ text: String) -> some View {
        HStack(spacing: 0) {
            Spacer(minLength: 48)
            Text(text)
                .font(.system(size: 15))
                .foregroundStyle(Palette.ink)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.horizontal, 13)
                .padding(.vertical, 9)
                .background(
                    RoundedRectangle(cornerRadius: 16, style: .continuous)
                        .fill(Palette.accentSoft)
                )
        }
    }

    private var workingRow: some View {
        HStack(spacing: 8) {
            ProgressView().controlSize(.small)
            Text("working…")
                .font(Typo.rowSub)
                .foregroundStyle(Palette.inkFaint)
        }
    }

    private var parkedRow: some View {
        HStack(spacing: 8) {
            Image(systemName: "hand.raised")
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(Palette.inkFaint)
            Text("waiting for your confirmation above")
                .font(Typo.rowSub)
                .foregroundStyle(Palette.inkFaint)
        }
    }

    /// The row STILL GROWING renders as plain text; every closed row is parsed
    /// once and memoized. `body` re-runs on every streamed token, so parsing the
    /// live row per render is a full markdown parse of an ever-longer string
    /// dozens of times a second, on the main actor — the formatting arrives the
    /// instant the turn closes, which is soon enough.
    private func assistantText(_ item: ChatItem) -> AttributedString {
        let streaming = session.running && session.transcript.last?.id == item.id
        return streaming
            ? AttributedString(item.text) : rendered.markdown(item.id, source: item.text)
    }

    // MARK: - the empty screen

    /// What an empty conversation says for itself. The three lines are TAPS THAT
    /// FILL THE FIELD, not taps that ask: a suggestion that fired a run on
    /// contact would spend the user's own key on a stray thumb. They are also
    /// the first thing to go when there is no key — a suggestion whose only
    /// destination is a field the gate has replaced is a dead end.
    private var opener: some View {
        VStack(alignment: .leading, spacing: 14) {
            VStack(alignment: .leading, spacing: 4) {
                Text("ask your inbox")
                    .font(Typo.serif(26, weight: .medium))
                    .foregroundStyle(Palette.ink)
                Text("It can search your mail, read a thread, and act once you say so.")
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.inkFaint)
                    .fixedSize(horizontal: false, vertical: true)
            }
            VStack(alignment: .leading, spacing: 8) {
                ForEach(keyMissing ? [] : Self.suggestions, id: \.self) { line in
                    Button {
                        question = line
                        focused = true
                    } label: {
                        HStack(spacing: 8) {
                            Text(line)
                                .font(.system(size: 14))
                                .foregroundStyle(Palette.inkDim)
                                .multilineTextAlignment(.leading)
                            Spacer(minLength: 6)
                            Image(systemName: "arrow.up.left")
                                .font(.system(size: 11, weight: .semibold))
                                .foregroundStyle(Palette.accent)
                        }
                        .padding(.horizontal, 13)
                        .padding(.vertical, 11)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .background(
                        RoundedRectangle(cornerRadius: 12, style: .continuous)
                            .fill(Palette.hairline.opacity(0.35))
                    )
                }
            }
        }
        .padding(.top, 8)
        .padding(.bottom, 6)
    }

    private static let suggestions = [
        "what needs an answer today?",
        "what did I say I would send this week?",
        "who is still waiting on me?",
    ]

    // MARK: - confirm card

    private func actionCard(_ action: PendingAction) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 7) {
                Image(systemName: action.tool.symbol)
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(Palette.accent)
                Text(Self.cardTitle(action))
                    .font(.system(size: 14, weight: .semibold))
                    .foregroundStyle(Palette.ink)
            }
            // WHO AND WHAT, FROM THE DAEMON. A send names its target inside the
            // preview instead (the recipient row), so these two are for the
            // cards that act on a message somebody else sent.
            if action.tool != .sendEmail {
                if let sender = action.verifiedSender { ComposeSummaryRow("from", sender) }
                if let subject = action.verifiedSubject { ComposeSummaryRow("subject", subject) }
            }
            if let detail = action.detail {
                Text(detail)
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.inkFaint)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if action.tool == .unsubscribeSender {
                Text(Self.unsubscribeNote)
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.inkFaint)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if action.tool == .sendEmail { sendPreview(action) }

            switch action.state {
            case .pending:
                cardButtons(action)
            case .running:
                HStack(spacing: 7) {
                    ProgressView().controlSize(.small)
                    Text("working…").font(Typo.rowSub).foregroundStyle(Palette.inkFaint)
                }
            default:
                Text(Self.outcomeLine(action.state))
                    .font(Typo.rowSub)
                    .foregroundStyle(
                        Self.isFailure(action.state) ? Palette.danger : Palette.inkFaintest)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .fill(Palette.accentSoft.opacity(0.6))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .strokeBorder(Palette.hairlineStrong, lineWidth: 0.75)
        )
    }

    /// THE TAP IS THE SECURITY BOUNDARY, so it gets the card's full width rather
    /// than the desktop's inline row of small buttons. A confirm the thumb can
    /// miss is a confirm somebody answers twice.
    private func cardButtons(_ action: PendingAction) -> some View {
        VStack(spacing: 8) {
            HStack(spacing: 8) {
                Button("Confirm") { session.resolve(action.id, .approved) }
                    .buttonStyle(.glassProminent)
                    .tint(Palette.accent)
                    .frame(maxWidth: .infinity)
                Button("Decline") { session.resolve(action.id, .declined) }
                    .buttonStyle(.glass)
                    .frame(maxWidth: .infinity)
            }
            if action.tool == .sendEmail {
                Button("Edit in composer") { editInComposer(action) }
                    .buttonStyle(.glass)
                    .frame(maxWidth: .infinity)
                    // Handing off REPLACES whatever the composer holds, so an
                    // open composition closes this door rather than losing what
                    // is in it. Same rule the Mac's card keeps.
                    .disabled(store.compose != nil)
            }
        }
        .controlSize(.large)
        .font(.system(size: 14, weight: .medium))
    }

    private func sendPreview(_ action: PendingAction) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            // ON A REPLY THE RECIPIENT IS THE PARENT'S SENDER, read back from
            // the daemon before this card was parked. The placeholder is the
            // last resort, not the normal case: the whole point of the tap is
            // that the person can see who the mail goes to.
            ComposeSummaryRow("to", action.to ?? action.verifiedSender ?? Self.derivedRecipient)
            // APPROVING A SEND MEANS SEEING EVERYONE IT REACHES. A copy list the
            // card did not mention is a recipient the person authorized without
            // knowing, and for a blind one this card is the only screen that
            // will ever say so — the sent mail shows it to nobody.
            if let cc = action.cc, !cc.trimmed.isEmpty { ComposeSummaryRow("cc", cc) }
            if let bcc = action.bcc, !bcc.trimmed.isEmpty { ComposeSummaryRow("bcc", bcc) }
            ComposeSummaryRow(
                "subject",
                action.subject
                    ?? action.verifiedSubject.map { "Re: \($0)" }
                    ?? (action.replyToMessageId != nil ? ComposeCopy.derivedSubject : "(none)"))
            // Taller than the Mac's 140pt well: this is the whole screen, and a
            // draft you are about to send under your own name should be readable
            // without a scroll inside a scroll wherever it can be.
            ScrollView {
                Text(Self.markdown(action.body ?? ""))
                    .font(.system(size: 14))
                    .foregroundStyle(Palette.ink)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .frame(maxHeight: 220)
            .padding(10)
            .background(
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .fill(Palette.canvas.opacity(0.6))
            )
        }
    }

    /// Hand the model's draft to the real composer and tell the agent it is no
    /// longer holding the pen. Same entry point every other compose flow uses,
    /// so the draft autosave and the send ceremony come along unchanged — and
    /// the sheet is already mounted on `store.compose` in MobileRootView, so
    /// there is nothing to close here the way the Mac's bar closes itself.
    private func editInComposer(_ action: PendingAction) {
        // `openCompose` overwrites `store.compose` outright AND cancels the
        // armed autosave, so a live composition would be gone from the screen
        // and from the server both. The button is disabled for the same reason;
        // this is the guard that makes that true rather than merely displayed.
        guard store.compose == nil else { return }
        store.openCompose(
            ComposeState(
                replyToMessageId: action.replyToMessageId,
                to: action.to ?? "",
                cc: action.cc ?? "",
                bcc: action.bcc ?? "",
                subject: action.subject ?? "",
                body: action.body ?? ""))
        session.resolve(action.id, .editedInComposer)
        focused = false
    }

    // MARK: - shown emails

    /// The show_emails cards: the agent's answer AS emails. Tapping one opens the
    /// thread through the same store verb every list row uses, and the reader
    /// pushes onto THIS tab's stack (MobileRootView's threadDestination), so the
    /// back chevron lands right back in the conversation that found it.
    private func emailCards(_ cards: [EmailCard]) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(cards) { card in
                Button {
                    store.openThread(card.threadId)
                } label: {
                    VStack(alignment: .leading, spacing: 4) {
                        HStack(spacing: 9) {
                            Avatar(sender: card.sender, size: 26)
                            Text(card.sender)
                                .font(.system(size: 14, weight: .semibold))
                                .foregroundStyle(Palette.ink)
                                .lineLimit(1)
                            Spacer(minLength: 6)
                            Text(Fmt.dateTime(card.date))
                                .font(Typo.num(11))
                                .foregroundStyle(Palette.inkFaintest)
                        }
                        Text(card.subject)
                            .font(.system(size: 14))
                            .foregroundStyle(Palette.inkDim)
                            .lineLimit(2)
                            .multilineTextAlignment(.leading)
                        if !card.snippet.isEmpty {
                            Text(card.snippet)
                                .font(Typo.rowSub)
                                .foregroundStyle(Palette.inkFaintest)
                                .lineLimit(2)
                                .multilineTextAlignment(.leading)
                        }
                    }
                    .padding(12)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .background(
                    RoundedRectangle(cornerRadius: 12, style: .continuous)
                        .fill(Palette.hairline.opacity(0.35))
                )
            }
        }
    }

    // MARK: - citations

    @ViewBuilder
    private func citations(_ cites: [ToolCitation]) -> some View {
        if !cites.isEmpty {
            VStack(alignment: .leading, spacing: 5) {
                Text("sources")
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                    .textCase(.uppercase)
                ForEach(cites) { citation in
                    Button {
                        store.openThread(citation.threadId)
                    } label: {
                        HStack(spacing: 9) {
                            Avatar(sender: citation.sender, size: 20)
                            VStack(alignment: .leading, spacing: 1) {
                                Text(citation.sender)
                                    .font(Typo.rowSub)
                                    .foregroundStyle(Palette.inkDim)
                                    .lineLimit(1)
                                Text(citation.subject)
                                    .font(Typo.micro)
                                    .foregroundStyle(Palette.inkFaintest)
                                    .lineLimit(1)
                            }
                            Spacer(minLength: 4)
                            Image(systemName: "arrow.up.right")
                                .font(.system(size: 11, weight: .semibold))
                                .foregroundStyle(Palette.accent)
                        }
                        // 44pt of row for a footnote-sized target.
                        .padding(.horizontal, 10)
                        .padding(.vertical, 9)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .background(
                        RoundedRectangle(cornerRadius: 10, style: .continuous)
                            .fill(Palette.hairline.opacity(0.22))
                    )
                }
            }
        }
    }

    // MARK: - the composer

    private var canSubmit: Bool { !question.trimmed.isEmpty && !session.running }

    /// The email behind the tab, as the agent should hear about it — nil when
    /// nothing is open. Read at ASK TIME (see `submit`), never held, so it can
    /// only ever name the thread that was open when the question was sent.
    private var openEmail: OpenEmailContext? {
        guard let threadId = store.threadId else { return nil }
        return OpenEmailContext(threadId: threadId, summary: store.currentThreadSummary)
    }

    /// What the "in:" line names, which is not the same email at every moment.
    /// While a run is open it names the email THAT RUN was asked under; idle, it
    /// names what the NEXT question would carry. Same rule as the Mac's chip, and
    /// it matters MORE here: the reader is a push on another tab, so the pinned
    /// thread is usually somewhere you cannot see from this screen.
    ///
    /// NEVER NIL WHILE A THREAD WOULD RIDE ALONG. `openEmail` attaches off
    /// `store.threadId`, but the subject arrives later, from the viewer's own
    /// fetch — and the chip is the only disclosure this screen has. In that
    /// window the chip says so generically rather than staying silent about an
    /// attachment the model can read.
    private var pinnedSubject: String? {
        if session.running {
            guard let active = session.activeAskEmail else { return nil }
            return active.summary?.subject ?? "the open email"
        }
        guard store.threadId != nil else { return nil }
        return store.currentThreadSummary?.subject ?? "the open email"
    }

    private var composer: some View {
        VStack(spacing: 0) {
            if let subject = pinnedSubject {
                HStack(spacing: 6) {
                    Image(systemName: "paperclip")
                        .font(.system(size: 10, weight: .semibold))
                        .foregroundStyle(Palette.inkFaintest)
                    Text("in: \(subject)")
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkDim)
                        .lineLimit(1)
                        .truncationMode(.tail)
                    Spacer(minLength: 0)
                }
                .padding(.horizontal, 18)
                .padding(.bottom, 6)
            }
            HStack(alignment: .bottom, spacing: 10) {
                // Never disabled: a long answer is exactly when somebody types
                // their next question. Only SEND waits for the turn to finish.
                //
                // A GROWING FIELD, so return makes a paragraph and the button
                // sends — the arrangement every messaging app on this phone
                // already taught the thumb.
                TextField("Ask about your mail", text: $question, axis: .vertical)
                    .lineLimit(1...5)
                    .font(.system(size: 16))
                    .foregroundStyle(Palette.ink)
                    .focused($focused)
                    .padding(.horizontal, 14)
                    .padding(.vertical, 10)
                    .background(
                        RoundedRectangle(cornerRadius: 20, style: .continuous)
                            .fill(Palette.hairline.opacity(0.5))
                    )
                Button(action: submit) {
                    Image(systemName: "arrow.up")
                        .font(.system(size: 16, weight: .bold))
                        .foregroundStyle(canSubmit ? Palette.canvas : Palette.inkFaintest)
                        .frame(width: 40, height: 40)
                        .background(
                            Circle().fill(canSubmit ? Palette.accent : Palette.hairline))
                }
                .buttonStyle(.plain)
                .disabled(!canSubmit)
                .accessibilityLabel("Send")
            }
            .padding(.horizontal, 16)
        }
        .padding(.top, 10)
        .padding(.bottom, 8)
        // The bar sits over the log, so it needs its own ground: a transparent
        // inset would let streamed text slide under the field it is answering.
        .background(.bar)
    }

    private func submit() {
        guard canSubmit else { return }
        let text = question.trimmed
        question = ""
        // The email goes WITH the question — read here, at ask time, so it can
        // never name a thread the user has since walked away from.
        session.send(text, openEmail: openEmail)
    }

    private func newConversation() {
        session.clear()
        question = ""
        rendered = MobileMarkdownCache()
        focused = false
    }

    // MARK: - the key gate

    /// Whether this ask would ride the daemon rather than a local key. The SAME
    /// resolution `AssistantSession.run` makes (pref AND capability), because a
    /// gate that disagreed with the loop would either refuse a question the
    /// daemon would have answered or wave through one nothing can.
    private var relaying: Bool {
        prefs.assistantTransport == .relay && store.relayAvailable
    }

    /// True once the keychain has answered and the answer cannot drive a run:
    /// no key at all, or a key for a provider this loop does not speak yet.
    ///
    /// RELAY HAS NO KEY TO MISS. On hosted, the credential lives on the daemon
    /// and this phone is never meant to hold one, so asking for a key there
    /// would turn the agent off for exactly the users it ships for.
    private var keyMissing: Bool {
        guard !relaying, let keyStatus else { return false }
        return !keyStatus.present || keyStatus.provider != .anthropic
    }

    /// Said BEFORE the first question rather than after it. Without a key the
    /// session answers every ask with the same error, which is a worse way to
    /// learn that the agent needs one — so the composer is replaced by the
    /// reason it would not have worked.
    private var keyGate: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 7) {
                Image(systemName: "key")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Palette.accent)
                Text("the agent is off")
                    .font(.system(size: 14, weight: .semibold))
                    .foregroundStyle(Palette.ink)
            }
            Text(gateLine)
                .font(Typo.rowSub)
                .foregroundStyle(Palette.inkFaint)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.horizontal, 18)
        .padding(.top, 12)
        .padding(.bottom, 10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.bar)
    }

    /// Names the door that fixes it, and the door is not the same on every
    /// install: where the daemon offers a relay, the cheapest fix is one tap on
    /// a switch rather than finding an API key, so the sentence says so. The
    /// path is Account and not Settings because that is where the panes live on
    /// the phone.
    private var gateLine: String {
        if relayAvailableForCopy {
            return keyStatus?.present == true
                ? "The agent speaks Anthropic for now. Under Account > Assistant, swap in a key that starts with sk-ant-, or switch chats to the Passband relay."
                : "Under Account > Assistant, switch chats to the Passband relay, or paste your own Anthropic API key."
        }
        return keyStatus?.present == true
            ? "The agent speaks Anthropic for now. Swap in a key that starts with sk-ant- under Account > Assistant."
            : "Paste your Anthropic API key under Account > Assistant to turn the agent on. It runs on your own key, from this phone."
    }

    /// The relay exists on this daemon but the user is on their own key. Not
    /// `relaying`: this gate only ever renders when relay is NOT carrying the
    /// ask, and the question here is whether the relay is available to offer.
    private var relayAvailableForCopy: Bool { store.relayAvailable }

    private func refreshKeyStatus() async {
        keyStatus = await AssistantKeyStore.statusAsync()
    }

    // MARK: - the question that opened this

    /// Send the words this screen was pushed with, ONCE. Sent rather than
    /// drafted because the push is the deliberate act: the ask row was tapped,
    /// or return was pressed on a question, and offering the same sentence back
    /// with a second Send to press is the app pretending it did not hear.
    ///
    /// The `asked` latch is not decoration. `onAppear` fires again every time
    /// the reader pops back off this stack (a citation, a shown email), and
    /// re-asking the opening question because somebody read one of its answers
    /// would spend the user's key on their back button.
    private func sendInitialQuestion() {
        guard !asked, let text = initialQuestion?.trimmed, !text.isEmpty else { return }
        asked = true
        // No key means no call: the words park in the composer under the key
        // gate, and the moment a key is pasted in Account they are still there
        // to send. onAppear awaits the keychain before calling this, so
        // `keyMissing` is a real answer here, not the optimistic nil default.
        guard !keyMissing else {
            question = text
            return
        }
        // A run is still open from the last visit: `send` would drop this on the
        // floor, so it lands in the composer instead and the Send button lights
        // up the moment the answer finishes. Losing a typed question silently is
        // the one outcome worth writing four lines to avoid.
        guard !session.running else {
            question = text
            return
        }
        session.send(text, openEmail: openEmail)
    }

    // MARK: - copy

    /// NATIVE markdown parsing, deliberately NOT the in-house `MarkdownStyle`:
    /// that scanner is the composer's editor highlighter and KEEPS the `**`
    /// visible, which is right over a source buffer and wrong in a chat log.
    /// Inline-only preserving whitespace, so a half-streamed list still reads as
    /// the lines it was typed as. A parse failure falls back to the raw text — a
    /// stray bracket must never blank an answer.
    fileprivate static func markdown(_ text: String) -> AttributedString {
        (try? AttributedString(
            markdown: text,
            options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace)))
            ?? AttributedString(text)
    }

    /// Only reached when a send names neither a recipient nor a parent we could
    /// read back — the daemon still derives one, but this card could not say so.
    private static let derivedRecipient = "(derived from the message being answered)"

    /// The part of an unsubscribe that happens outside Passband, said before the
    /// tap rather than after it.
    private static let unsubscribeNote =
        "Opens this sender's unsubscribe page in your browser."

    private static func cardTitle(_ action: PendingAction) -> String {
        switch action.tool {
        case .archiveMessage: "Archive this email"
        case .labelMessage: "Change this email's labels"
        case .sendEmail: "Send this email"
        case .unsubscribeSender: "Unsubscribe from this sender"
        default: action.verb
        }
    }

    private static func outcomeLine(_ state: PendingAction.State) -> String {
        switch state {
        case .declined: "Declined"
        case .handedOff: "Moved to the composer"
        case .executed(let done): done
        case .failed(let text): text
        case .pending, .running: ""
        }
    }

    private static func isFailure(_ state: PendingAction.State) -> Bool {
        if case .failed = state { return true }
        return false
    }
}

/// Parsed markdown, memoized per transcript row.
///
/// A CLASS, held by `@State`, so `row(_:)` can fill it during body evaluation —
/// a value type there would be a mutation mid-render. Deliberately NOT
/// observable: filling the memo must not invalidate the view that just read it.
/// Keyed on the row id AND its source text, so a row that grows past the
/// streaming window still re-parses exactly once when it settles.
@MainActor
private final class MobileMarkdownCache {
    private var entries: [ChatItem.ID: (source: String, rendered: AttributedString)] = [:]

    func markdown(_ id: ChatItem.ID, source: String) -> AttributedString {
        if let hit = entries[id], hit.source == source { return hit.rendered }
        let parsed = MobileAgentView.markdown(source)
        entries[id] = (source, parsed)
        return parsed
    }
}

/// One tool-activity chip: what the agent is doing, with its outcome on the
/// trailing edge. A step up in type from the Mac's 10pt version — this one is
/// read at arm's length rather than at a desk.
private struct MobileToolChip: View {
    let tool: ToolActivity

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: AgentTools.Tool(rawValue: tool.name)?.symbol ?? "wrench")
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(Palette.inkFaint)
                .frame(width: 15)
            Text(tool.summary)
                .font(Typo.rowSub)
                .foregroundStyle(Palette.inkFaint)
                .lineLimit(1)
                .truncationMode(.tail)
            Spacer(minLength: 4)
            switch tool.state {
            case .running:
                ProgressView().controlSize(.small)
            case .ok:
                Image(systemName: "checkmark")
                    .font(.system(size: 11, weight: .bold))
                    .foregroundStyle(Palette.positive.opacity(0.8))
            case .failed:
                Image(systemName: "xmark")
                    .font(.system(size: 11, weight: .bold))
                    .foregroundStyle(Palette.danger)
            }
        }
    }
}
