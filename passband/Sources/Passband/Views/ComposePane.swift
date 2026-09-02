// The send ceremony — the one irreversible action in the app, so it gets two
// phases: edit (⌘Enter → review), then review, which submits once *without*
// override_guard to get the outbound-guard verdict. A clean pass has already
// sent; a 422 shows the redacted guard kinds and demands a distinct override
// (shift+Enter or the danger button). 403 means no write credential.
//
// This is the PANE composer: a right-hand working surface in MainShell's
// layout, half the window wide — the page beside it shrinks and stays live,
// because starting an email should not mean losing sight of the inbox that
// prompted it. No scrim, no blur; Esc closes it like the side panels. Replies
// open the reader's inline composer (InlineReply), which runs the same ceremony
// against the same `ComposeSubmit`; this pane owns the new-message path
// (`replyToMessageId == nil`), plus the reply shape it still supports for any
// caller with no thread to open.
//
// The body is markdown, styled LIVE with the markers kept visible (see
// MarkdownTextView); the daemon renders the HTML half of what actually goes
// out from this same source (`body_format: "markdown"`).
//
// ON A PHONE THE PANE IS A SHEET, and that is the only structural difference:
// MobileRootView presents this same view at `.large` off the same
// `store.compose`, so opening, restoring, autosaving and sending are one code
// path on both platforms. What is fenced below is the DESKTOP FURNITURE only —
// the key hints (there is no Esc to promise), the pane's glass edge and its
// leftward shadow (a sheet brings its own ground), and the labels that named
// keys. The ceremony itself, both phases of it, is shared and untouched: a
// phone drives it with the footer buttons that were always there beside the
// hints.

import SwiftUI

struct ComposePane: View {
    @Environment(AppStore.self) private var store
    @FocusState private var focusedField: FocusTarget?

    private enum FocusTarget: Hashable { case recipient(RecipientSlot), subject }

    @State private var groupPickerOpen = false
    /// The picked group's size, for the fan-out pill's "· 12 ·". Held here rather
    /// than on ComposeState because it is a display detail the daemon re-reads
    /// from `groupId` anyway.
    @State private var groupMemberCount = 0

    private var compose: ComposeState? { store.compose }
    private var inReview: Bool { compose?.phase == .review }
    private var guarded: Bool { !(compose?.guardKinds.isEmpty ?? true) }

    var body: some View {
        if let compose {
            VStack(alignment: .leading, spacing: 0) {
                header(compose)

                VStack(alignment: .leading, spacing: 12) {
                    if inReview {
                        reviewPane(compose)
                    } else {
                        editPane(compose)
                    }
                    if let error = compose.error {
                        Text(error).font(Typo.micro).foregroundStyle(Palette.danger)
                    }
                }
                .padding(.horizontal, 18)
                .padding(.vertical, 14)
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)

                // EDIT HAS NO FOOTER ON A PHONE. Its two buttons went to the
                // header and to the drag-down gesture, and a bar left behind
                // with nothing in it would still cost its own height and
                // hairline between the editor and the keyboard.
                #if os(macOS)
                    footer(compose)
                #else
                    if inReview { footer(compose) }
                #endif
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            // A pane is an EDGE in a window: glass, and a shadow thrown left
            // over the page it half-covers. A sheet is neither — it has its own
            // presented shape and nothing beside it to cast onto — so the phone
            // takes the plain canvas the rest of its surfaces stand on.
            #if os(macOS)
                .passbandGlass(.pane, cornerRadius: 0, tint: Palette.glassTintStrong)
                .shadow(color: .black.opacity(0.24), radius: 40, x: -14)
            #else
                .background(Palette.canvas)
            #endif
            .keyContext(.modal)
            .keyBindings(.modal, bindings)
            .onAppear { if !inReview { focusedField = .recipient(.to) } }
        }
    }

    private func header(_ compose: ComposeState) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            // THE SERIF MOMENT. On the Mac this line is one engraved label among
            // the rail, the zone heads and the page chrome, and a second display
            // face there would be wallpaper. A sheet has no chrome around it —
            // it IS the screen — so the phone spends Newsreader here, once, on
            // the word that names what you are doing.
            #if os(macOS)
                Text(inReview ? "review · confirm send" : "compose")
                    .font(Typo.sectionLabel)
                    .foregroundStyle(Palette.ink)
                    .textCase(.uppercase)
            #else
                Text(inReview ? "review" : "compose")
                    .font(Typo.serif(24, weight: .medium))
                    .foregroundStyle(Palette.ink)
            #endif
            Text(kindLabel(compose))
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
            Spacer()
            // The key chip is the Mac's promise that a key does this.
            #if os(macOS)
                HStack(spacing: 4) {
                    Kbd("Esc")
                    Text("close").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                }
            #else
                // THE PHONE'S PRIMARY ACTION, IN THE CORNER IT LIVES IN. On a
                // phone the edit phase is a sheet with the keyboard up, and the
                // bottom of the screen is spoken for three times over — the
                // keyboard, its predictive row, and the markdown bar above both.
                // A footer under all of that is the one strip of chrome the
                // thumb has to reach PAST its own keyboard to use, so the verb
                // moves to the bar, where every other phone puts it.
                //
                // AND CANCEL IS NOT BESIDE IT, because the sheet already is
                // cancel: a drag down runs `closeCompose()` through the same
                // binding the button called, and the drag indicator is showing.
                // Two doors out, one of them costing a corner of the bar, was a
                // button spent on something the gesture already did.
                //
                // Review keeps its footer: no keyboard is up there, and `send`
                // is the one irreversible thing in the app — it stays a wide,
                // deliberate target rather than a chip in the corner.
                if !inReview {
                    Button("review →") { toReview() }
                        .buttonStyle(.glassProminent)
                        .tint(Palette.accent)
                }
            #endif
        }
        .padding(.horizontal, 18)
        .padding(.vertical, 13)
        .overlay(alignment: .bottom) { Hairline() }
    }

    /// Which of the three this composer is, in the header's lowercase voice.
    private func kindLabel(_ compose: ComposeState) -> String {
        if compose.forwardOfMessageId != nil { return "forward" }
        return compose.replyToMessageId != nil ? "reply" : "new message"
    }

    // MARK: - the forwarded original

    /// How tall the quote may grow in the EDIT phase before it starts scrolling
    /// inside itself. One number for both platforms on purpose: it is a CEILING,
    /// not a height — the editor above is greedy too, so a short pane (or a
    /// phone sheet with the keyboard up) splits the space between them and the
    /// quote simply gets less than this. Review has no ceiling at all; there is
    /// nothing to type into by then, so the whole thing scrolls with the note.
    private static let quoteEditHeight: CGFloat = 300

    /// WHAT RIDES ALONG on a forward, in both phases: the message itself,
    /// indented behind a rail the way every mail client draws included mail.
    ///
    /// The real quote is assembled by the DAEMON out of its own raw fetch, so
    /// nothing here is on the wire — this is the reader's sanitized copy of the
    /// same message (see `ComposeState.forwardedMessage`), shown so the composer
    /// is not an empty new message that inexplicably sends a fat email, and so
    /// review, whose whole job is promising what goes out, promises the whole of
    /// it rather than the covering note.
    ///
    /// NO `to` OR `cc` LINES, and their absence is a fact about the client
    /// rather than a choice: `ClientMessage` carries the sender and nothing else
    /// about the audience, while the daemon's block writes `To:` and `Cc:` from
    /// the raw headers. Inventing them from what is in reach would be inventing
    /// recipients, and the one question a forwarded header block exists to
    /// answer is who was on it.
    @ViewBuilder
    private func forwardedQuote(_ compose: ComposeState) -> some View {
        if compose.forwardOfMessageId != nil, let message = compose.forwardedMessage {
            VStack(alignment: .leading, spacing: 9) {
                // The composer's mirror of the wire's
                // "---------- Forwarded message ---------" banner: the same
                // sentence the outgoing mail carries, said in the house's micro
                // voice instead of in a row of hyphens.
                Text("forwarded message")
                    .font(Typo.micro)
                    .foregroundStyle(Palette.inkFaintest)
                    .textCase(.uppercase)

                // The header lines of that banner, in review's own summary
                // grammar — a header being checked, in mono. The subject comes
                // off `forwardedSubject` rather than off the message, because
                // that is the value the outgoing `Fwd: …` title was built from
                // and the two must not read differently.
                VStack(alignment: .leading, spacing: 3) {
                    ComposeSummaryRow("from", message.senderString)
                    ComposeSummaryRow("date", Fmt.dateTime(message.received_at))
                    ComposeSummaryRow("subject", compose.forwardedSubject ?? "(no subject)")
                }

                // THE READER'S OWN BODY VIEWS, picked by the reader's own test
                // (MessageCard) — html when there is any, plain text otherwise.
                // Same `cacheKey` the reader passes, so this shares the frame
                // pool and the image cache with the thread behind the pane
                // rather than fetching every picture a second time, and the same
                // tracker policy, so opening a composer cannot load a pixel the
                // reader refused.
                if let html = message.html, !html.isEmpty {
                    EmailWebView(
                        html: html, cacheKey: String(message.id),
                        allowTrackers: message.allowsTrackers)
                } else {
                    PlainBody(content: message.content)
                }

                // Unconditional, exactly as the reader mounts it: the strip
                // draws nothing at all when there are no files.
                AttachmentStrip(attachments: message.attachmentList)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            // INDENTED LIKE QUOTED MAIL: the whole block sits behind a static
            // bar in the hairline token. Same idiom as the reader's selection
            // rail, minus the only thing that rail does — this one never moves
            // and never changes color, because it marks a kind of content
            // rather than where you are.
            .padding(.leading, 12)
            .overlay(alignment: .leading) {
                RoundedRectangle(cornerRadius: 1, style: .continuous)
                    .fill(Palette.hairlineStrong)
                    .frame(width: 2)
            }
        }
    }

    private func footer(_ compose: ComposeState) -> some View {
        HStack(spacing: 8) {
            #if os(macOS)
                hint
            #endif
            Spacer()
            if inReview {
                Button(ComposeLabels.back) { patch { $0.phase = .edit; $0.error = nil } }
                    .buttonStyle(.glass)
                if guarded {
                    Button(compose.sending ? "sending…" : "override + send") {
                        Task { await fire(override: true) }
                    }
                    .buttonStyle(.glassProminent)
                    .tint(Palette.danger)
                    .disabled(compose.sending)
                } else {
                    Button(compose.sending ? "sending…" : "send") {
                        Task { await fire(override: false) }
                    }
                    .buttonStyle(.glassProminent)
                    .tint(Palette.accent)
                    .disabled(compose.sending)
                }
            } else {
                // EDIT PHASE, DESKTOP ONLY — the phone never renders this bar
                // (see the call site) and must not start: the tracker switch is
                // what a phone would have to give the row for, and one switch
                // nobody came here to touch is not worth a strip of chrome
                // between the editor and the keyboard. A phone that wants the
                // pixel changes the account default; review still SAYS when one
                // is armed, so a tracked send is never a silent one.
                #if os(macOS)
                    // What goes out is settled by the time review is up, and a
                    // switch beside the send button is a switch nobody meant to
                    // touch.
                    TrackerToggle(on: bindFlag(\.includeTracker))
                    Button(ComposeLabels.cancel) { store.closeCompose() }
                        .buttonStyle(.glass)
                    Button("review →") { toReview() }
                        .buttonStyle(.glassProminent)
                        .tint(Palette.accent)
                #endif
            }
        }
        .padding(.horizontal, 18)
        .padding(.vertical, 12)
        .overlay(alignment: .top) { Hairline() }
    }

    private func editPane(_ compose: ComposeState) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            recipientRow(compose)
            Field(label: "subject") {
                // Left blank on a reply the daemon titles from the parent; the
                // placeholder says so, because an empty field otherwise reads as
                // an unset required value.
                TextField(subjectPlaceholder, text: bind(\.subject))
                    .textFieldStyle(.plain)
                    .focused($focusedField, equals: .subject)
            }
            VStack(alignment: .leading, spacing: 5) {
                HStack(spacing: 6) {
                    Text("body").font(Typo.micro).foregroundStyle(Palette.inkFaint)
                    Text("markdown: **bold**, *italic*, `code`, [links](url)")
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkFaintest)
                        .lineLimit(1)
                        .minimumScaleFactor(0.85)
                }
                MarkdownTextView(text: bind(\.body))
                    .frame(maxHeight: .infinity)
                    .padding(8)
                    .background(
                        RoundedRectangle(cornerRadius: 9, style: .continuous)
                            .fill(Palette.canvas.opacity(0.65))
                    )
                    .overlay(
                        RoundedRectangle(cornerRadius: 9, style: .continuous)
                            .strokeBorder(Palette.hairlineStrong, lineWidth: 0.75))
            }
            .frame(maxHeight: .infinity)

            // UNDER the editor, where the quote sits in the mail itself, and in
            // a scroller of its own: the note is what you are writing and keeps
            // the flexible remainder, while the original — which can be a whole
            // newsletter — scrolls inside a bounded pocket instead of pushing
            // the editor off the pane.
            //
            // The EDITOR is deliberately not the thing wrapped: MarkdownTextView
            // is a platform text view that scrolls itself, and nesting it in a
            // ScrollView breaks its own scrolling.
            if compose.forwardOfMessageId != nil {
                ScrollView { forwardedQuote(compose) }
                    .frame(maxHeight: Self.quoteEditHeight)
            }
        }
    }

    /// The `to` line and the two affordances that live beside it: browse groups,
    /// and open the bcc row.
    ///
    /// A REPLY GETS NEITHER. A group is an audience and a reply already has one;
    /// the daemon refuses the combination outright, so offering the button here
    /// would be offering a refusal.
    @ViewBuilder
    private func recipientRow(_ compose: ComposeState) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            // `to`, with cc and bcc folded behind their own labels on its line
            // — and unfolded on their own whenever they hold anybody. The group
            // affordances belong to `to` alone: a group is an audience, and an
            // audience is who the mail is TO. See `RecipientFields`.
            RecipientFields(
                recipients: recipientsBinding, focus: $focusedField,
                field: FocusTarget.recipient,
                suggestGroups: canAddressGroup,
                onGroupPicked: { pick($0) },
                resolvedGroup: compose.groupName.map {
                    (name: $0, count: groupMemberCount)
                })
            if canAddressGroup {
                HStack(spacing: 8) {
                    Button {
                        groupPickerOpen = true
                    } label: {
                        HStack(spacing: 4) {
                            Image(systemName: "person.2").font(.system(size: 9, weight: .semibold))
                            Text("groups").font(Typo.micro)
                        }
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .foregroundStyle(Palette.inkFaint)
                    .popover(isPresented: $groupPickerOpen, arrowEdge: .bottom) {
                        GroupPicker { group in
                            groupPickerOpen = false
                            pick(group)
                        }
                    }
                    // The bcc toggle used to live here. It is on the `to`
                    // field's own label row now, beside cc, where both fields
                    // are unfolded from — and a bcc group still opens the row
                    // on its own by filling it.
                    Spacer()
                }
            }
        }
    }

    /// Groups address a NEW message. A reply and a forward each already have an
    /// audience, and the daemon refuses a `group_id` on either.
    private var canAddressGroup: Bool {
        compose?.replyToMessageId == nil && compose?.forwardOfMessageId == nil
    }

    /// What picking a group does, and the whole of the mode's meaning in this
    /// composer:
    ///
    /// * `to` / `bcc` — EXPAND into that field now, as ordinary address pills.
    ///   What goes on the wire is real recipients, so what the sender reviews is
    ///   exactly who it reaches. The group id rides along as attribution only.
    /// * `individual` — ONE pill, because there is no single message to address.
    ///   The daemon reads the membership itself; `to` carries only the token that
    ///   keeps the pill alive across a draft round-trip.
    private func pick(_ group: SendGroup) {
        Task {
            // The list read carries counts, not membership, so to/bcc need the
            // full group before they can expand it.
            let full = (try? await APIClient.shared.group(group.id)) ?? group
            let members = (full.members ?? []).map(\.addr)
            patch { state in
                state.groupId = full.id
                state.groupMode = full.mode
                state.groupName = full.name
                switch full.mode {
                case .to:
                    state.to = merge(state.to, members)
                case .bcc:
                    // No reveal flag to set: `RecipientFields` unfolds any row
                    // that holds somebody, which is the same rule that keeps a
                    // restored draft's bcc from hiding itself.
                    state.bcc = merge(state.bcc, members)
                case .individual:
                    // The token REPLACES whatever was in the field. A fan-out
                    // addresses one audience and nobody else: leaving a typed
                    // address beside it would be a second, silent recipient of a
                    // mail whose whole point is that it is one-to-one.
                    state.to = GroupToken.encode(full)
                }
            }
            groupMemberCount = full.member_count
            DraftSaver.shared.noteChange(.compose)
        }
    }

    /// Add addresses to a comma-joined field without duplicating what is there.
    private func merge(_ existing: String, _ addrs: [String]) -> String {
        var out = existing.split(separator: ",").map { String($0).trimmed }
            .filter { !$0.isEmpty }
        let seen = Set(out.map { $0.lowercased() })
        for addr in addrs where !seen.contains(addr.lowercased()) {
            out.append(addr)
        }
        return out.joined(separator: ", ")
    }

    /// The `to` row in review. A fan-out's field holds only a token, which is a
    /// client encoding and not something to show a person; the group's own name
    /// is what they picked and what they should be asked to confirm.
    private func reviewTo(_ compose: ComposeState) -> String {
        if compose.groupMode == .individual, let name = compose.groupName {
            return name
        }
        return compose.to.isEmpty ? "(none)" : compose.to
    }

    /// How this send is about to happen, when a group decided it. nil for an
    /// ordinary message, whose fields already say everything.
    private func reviewShape(_ compose: ComposeState) -> String? {
        guard let mode = compose.groupMode, let name = compose.groupName else { return nil }
        switch mode {
        case .to:
            return "\(name) · everyone can see the whole list"
        case .bcc:
            return "\(name) · nobody sees who else got it"
        case .individual:
            let n = groupMemberCount
            return n > 0
                ? "\(n) separate emails, one per person in \(name)"
                : "one separate email per person in \(name)"
        }
    }

    private var isReply: Bool { compose?.replyToMessageId != nil }

    /// Stands in for an empty subject on a reply, in both panes: the daemon titles
    /// it `Re: <parent subject>`, and the real parent subject is not in reach here
    /// (the update carries an LLM summary, not the header).
    private var subjectPlaceholder: String {
        isReply ? ComposeCopy.derivedSubject : "subject"
    }

    /// WHAT THE MAIL WILL BE TITLED, which is not always what is in the subject
    /// field: review's whole job is promising what goes out, so an empty field
    /// has to show the title the DAEMON will derive rather than a blank row.
    ///
    /// The empty test is `trimmed.isEmpty`, because that is the DAEMON's test:
    /// `action_send` filters the subject through `!s.trim().is_empty()` before
    /// falling back to its derivation, so a field holding only spaces hands
    /// titling back to the daemon exactly as a cleared one does. (`fire` sends
    /// the spaces, and the daemon discards them — the wire carries them, the
    /// mail never does.) Testing `isEmpty` here promised a blank title for a
    /// mail about to go out titled `Fwd: …`, one whitespace away from the lie
    /// this function exists to remove.
    ///
    /// Three empty cases, and they derive differently:
    /// - a reply: `gmail_write::reply_subject` from the parent, which the
    ///   composer's update cannot see (it carries an LLM summary, not headers).
    /// - a FORWARD: `gmail_write::forward_subject` of the original, which we CAN
    ///   mirror, because the original's subject is right here as
    ///   `forwardedSubject`. Reachable exactly when the sender cleared the field
    ///   the composer opened pre-filled.
    /// - a new message: nothing derives one; it goes out untitled.
    private func reviewSubject(_ compose: ComposeState) -> String {
        guard compose.subject.trimmed.isEmpty else { return compose.subject }
        if compose.forwardOfMessageId != nil {
            return ComposeCopy.forwardSubject(compose.forwardedSubject ?? "")
        }
        return isReply ? ComposeCopy.derivedSubject : "(none)"
    }

    private func reviewPane(_ compose: ComposeState) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            ComposeSummaryRow("to", reviewTo(compose))
            // Only when there is one, and BOTH when there are — review's whole
            // job is stating what goes out, and a blind copy is the part of that
            // the sent mail will not show anybody, including the sender.
            if !compose.cc.trimmed.isEmpty {
                ComposeSummaryRow("cc", compose.cc)
            }
            if !compose.bcc.trimmed.isEmpty {
                ComposeSummaryRow("bcc", compose.bcc)
            }
            // THE SHAPE OF THE SEND, said out loud, and only when a group made it
            // something other than one message to the people listed above. This
            // is the one irreversible action in the app and the mode is the part
            // of it that the fields cannot show: twelve separate emails and one
            // bcc'd email look identical up there and are nothing alike.
            if let shape = reviewShape(compose) {
                ComposeSummaryRow("sending", shape)
            }
            ComposeSummaryRow("subject", reviewSubject(compose))
            // Review states what is about to go out, and an invisible pixel in
            // it is part of that. Only when armed: a row saying "no" on every
            // ordinary send is a row nobody reads.
            if compose.includeTracker && store.trackingAvailable {
                ComposeSummaryRow("tracking", ComposeCopy.trackedSend)
            }

            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    // The scanner's own styling, so review shows the formatting
                    // the HTML half will carry — not a second interpretation of
                    // it.
                    Text(MarkdownStyle.attributed(compose.body))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    // INSIDE this scroller rather than beside it, and under the
                    // note exactly as it will sit in the mail: on a forward the
                    // quote is most of what goes out, and review's whole job is
                    // stating what goes out. No inner ScrollView here — one
                    // scroll surface, the outer one.
                    forwardedQuote(compose)
                }
            }
            .frame(maxHeight: .infinity)
            .padding(10)
            .background(
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .fill(Palette.canvas.opacity(0.6))
            )

            // THE GUARD SPEAKS ONLY WHEN IT HAS SOMETHING TO SAY. This slot used
            // to carry a standing line announcing that the outbound secret scan
            // had not run yet and that sending is what runs it — true, and no use
            // to anybody: it named an internal subsystem, promised a "verdict" on
            // a trial the reader never knew about, and said "not yet checked"
            // about the one thing they could not check. A review pane's job is to
            // state what goes out, and "a thing you have not heard of has not
            // happened yet" is not part of that.
            //
            // Nothing about the scan changed. It still runs daemon-side on the
            // send itself (squelch-api/src/guard.rs), a clean pass still means the
            // mail is already gone, and a match still stops the send dead and
            // brings up the box below with the redacted kinds and the override.
            // The mechanism was never what needed explaining on a clean draft.
            if !compose.guardKinds.isEmpty {
                GuardVerdictBox(kinds: compose.guardKinds)
            }
        }
    }

    @ViewBuilder
    private var hint: some View {
        HStack(spacing: 4) {
            if inReview {
                Kbd("esc")
                Text("back").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                Text("·").foregroundStyle(Palette.inkFaintest)
                if guarded {
                    Kbd("shift+enter")
                    Text("override + send")
                        .font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                } else {
                    Kbd("enter")
                    Text("send").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                }
            } else {
                Kbd("esc")
                Text("cancel").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                Text("·").foregroundStyle(Palette.inkFaintest)
                Kbd("⌘enter")
                Text("review").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
            }
        }
    }

    // MARK: - keymap

    /// True while the caret sits in the body editor. The body is a platform text
    /// view (FocusState cannot see into AppKit or UIKit), and plain Enter there
    /// must stay a newline.
    ///
    /// `@FocusState` is deliberately NOT the answer on either side: it tracks
    /// the SwiftUI fields above (to, subject) and has no opinion about a
    /// representable's first responder. AppKit is asked directly; UIKit
    /// publishes no current-responder API, so the editors report themselves into
    /// `MarkdownFocus` and this reads that. Same fact, same freshness — dynamic
    /// on every evaluation, never a flag someone has to remember to clear.
    private var bodyHasFocus: Bool {
        #if os(macOS)
            NSApp.keyWindow?.firstResponder is NSTextView
        #else
            MarkdownFocus.shared.isEditing
        #endif
    }

    private var bindings: [KeyBinding] {
        [
            KeyBinding("Escape", inReview ? "back to edit" : "cancel", allowInInput: true) {
                if store.compose?.phase == .review {
                    patch { $0.phase = .edit; $0.error = nil }
                } else {
                    store.closeCompose()
                }
            },
            // In the body field plain Enter is a newline; ⌘Enter reviews. In
            // review, Enter fires without override — that call is the verdict.
            KeyBinding(declining: "Enter", inReview ? "send" : "review", allowInInput: true) {
                guard let compose = store.compose else { return false }
                // THE ASK BAR IS A MODAL ON TOP OF THIS PANE. KeyMonitor walks
                // the sets newest-first and AskBar binds only Escape, so a
                // Return typed into its field falls through to here — and in
                // review that would SEND THE MAIL, the one irreversible thing
                // this app does, instead of asking the question. The edit branch
                // survives on the firstResponder check below; review has no such
                // luck, so both decline while the bar is up.
                guard !store.askBarOpen else { return false }
                if compose.phase == .edit {
                    guard !bodyHasFocus else { return false }  // let it type a newline
                    toReview()
                } else {
                    Task { await fire(override: false) }
                }
                return true
            },
            KeyBinding("Enter", "review", meta: true, allowInInput: true) {
                if store.compose?.phase == .edit { toReview() }
            },
            // Explicit override, review phase only.
            KeyBinding(declining: "shift+Enter", "override guard and send", allowInInput: true) {
                // Same trap as plain Enter, one notch worse: this one sends
                // THROUGH the outbound guard.
                guard !store.askBarOpen else { return false }
                guard let compose = store.compose, compose.phase == .review,
                    !compose.guardKinds.isEmpty
                else { return false }
                Task { await fire(override: true) }
                return true
            },
        ]
    }

    // MARK: - state helpers

    /// The three recipient fields as one binding. Writes go through
    /// `stateRecipients`, which records that this composer's fields ARE the
    /// audience — see `ComposeState.recipientsStated`. Autosaves like every
    /// other field: a Bcc typed and then abandoned has to come back.
    private var recipientsBinding: Binding<Recipients> {
        Binding(
            get: { store.compose?.recipients ?? Recipients() },
            set: { value in
                guard store.compose?.recipients != value else { return }
                patch { $0.stateRecipients(value) }
                DraftSaver.shared.noteChange(.compose)
            })
    }

    private func bind(_ keyPath: WritableKeyPath<ComposeState, String>) -> Binding<String> {
        Binding(
            get: { store.compose?[keyPath: keyPath] ?? "" },
            set: { value in
                // Every field of this composer is bound through here, which is why
                // the autosave hooks HERE and nowhere else: there is no way to edit
                // the draft without arming a save.
                guard store.compose?[keyPath: keyPath] != value else { return }
                patch { $0[keyPath: keyPath] = value }
                DraftSaver.shared.noteChange(.compose)
            })
    }

    /// Same shape as `bind`, minus the autosave: a draft records what was
    /// written, not how the next send is addressed.
    private func bindFlag(_ keyPath: WritableKeyPath<ComposeState, Bool>) -> Binding<Bool> {
        Binding(
            get: { store.compose?[keyPath: keyPath] ?? false },
            set: { value in patch { $0[keyPath: keyPath] = value } })
    }

    private func patch(_ mutate: (inout ComposeState) -> Void) {
        guard var next = store.compose else { return }
        mutate(&next)
        store.compose = next
    }

    /// Patch the slot ONLY IF it still holds the composer `id` names. Every
    /// write that happens after an `await` goes through here: the plain `patch`
    /// above is safe only because its callers run synchronously off a keystroke,
    /// while a send's continuation resumes into whatever the slot holds by then,
    /// which may be an entirely different draft (see `ComposeState.id`).
    /// Silently does nothing in that case, by design — the composer that moved
    /// on is not this send's to edit, and the mail may well have gone out.
    private func patch(_ id: UUID, _ mutate: (inout ComposeState) -> Void) {
        guard var next = store.compose, next.id == id else { return }
        mutate(&next)
        store.compose = next
    }

    private func toReview() {
        guard let compose = store.compose else { return }
        // Untouched covers the seeded signature: a signature under nothing is
        // not a message, and review must not put it one Enter from going out.
        //
        // A FORWARD IS EXEMPT, and has to be: its content is the original the
        // daemon quotes underneath, so "here, look at this" with nothing typed
        // above it is the ordinary case rather than an empty message. The wire
        // agrees — `forward_of_message_id` is the one shape that accepts an
        // empty body — and refusing it here would make the bare forward, which
        // is most of them, unsendable.
        guard compose.forwardOfMessageId != nil || !Prefs.shared.isBodyUntouched(compose.body)
        else {
            patch { $0.error = "body is empty" }
            return
        }
        patch {
            $0.phase = .review
            $0.error = nil
            $0.guardKinds = []
        }
    }

    /// The request lives in `ComposeSubmit`; what is left here is this surface's
    /// own reaction to each outcome.
    ///
    /// EVERY WRITE BELOW IS KEYED TO `slot`, the composer this send belongs to.
    /// `store.compose` is a slot, not an object: while the await is out the
    /// sender can Escape (which flushes the draft and empties the slot) and open
    /// a different composer into it, and an unkeyed continuation would then land
    /// on a stranger's draft — `noteSent` clearing its touched mark, so the
    /// close's flush refuses to save, so everything typed into it is gone. See
    /// `ComposeState.id`.
    private func fire(override: Bool) async {
        guard let compose = store.compose, !compose.sending else { return }
        let slot = compose.id
        patch(slot) {
            $0.sending = true
            $0.error = nil
        }
        switch await ComposeSubmit.fire(compose, override: override) {
        case .sent:
            // The daemon resolved the replied-to update; without this the row
            // sits in its band until the next poll, reading as a no-op. No undo
            // pairs with it — a send is the one irreversible action.
            //
            // These two are facts about the MAIL, not about the slot, so they
            // stand whoever holds the composer now: it really went out, and the
            // person who sent it is owed the toast that says so.
            if let repliedTo = compose.replyToMessageId { store.noteResolved(repliedTo) }
            store.pushToast("sent", .success)
            // The slot half. The send already deleted the draft it carried, so
            // without `noteSent` the close would flush it straight back and
            // offer to restore mail that is gone — but only for THIS composer.
            // If the slot moved on, both of these belong to someone else's
            // draft and the right amount of work to do is none.
            if store.compose?.id == slot {
                DraftSaver.shared.noteSent(.compose)
                store.closeCompose()
            }
        case .guardBlocked(let kinds):
            // Stay in review with the redacted verdict; the override must be an
            // explicit second act, not a re-fire of the same call.
            patch(slot) {
                $0.phase = .review
                $0.guardKinds = kinds
                $0.sending = false
                $0.error = nil
            }
        case .forbidden:
            patch(slot) {
                $0.sending = false
                $0.error = ComposeCopy.noWriteCredential
            }
        case .failure(let text):
            patch(slot) {
                $0.sending = false
                $0.error = text
            }
        }
    }
}

// MARK: - labels

/// The two buttons whose LABEL names a key. Both composers say them, and on a
/// phone the key half is a promise nothing can keep — there is no Esc — so the
/// verb stands alone rather than teaching a shortcut that does not exist.
enum ComposeLabels {
    #if os(macOS)
        static let back = "esc back"
        static let cancel = "esc cancel"
        static let dismiss = "esc dismiss"
    #else
        static let back = "back"
        static let cancel = "cancel"
        // "dismiss", never "discard": closing a composer FLUSHES its draft, so
        // the reply is kept and restored next time, not thrown away.
        static let dismiss = "dismiss"
    #endif
}

// MARK: - shared review chrome

/// THE outbound-guard verdict, rendered identically wherever a reply started.
/// The one screen whose job is talking a reader out of a mistake must not read
/// differently in the pane composer and in the reader's inline one.
struct GuardVerdictBox: View {
    /// The redacted kinds the guard matched. Never rendered as markup — they are
    /// server strings.
    let kinds: [String]

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 4) {
                Text("outbound guard blocked · matched (redacted):")
                    .font(Typo.micro)
                Text(kinds.joined(separator: ", "))
                    .font(Typo.mono(11, weight: .semibold))
            }
            .foregroundStyle(Palette.danger)
            Text("review the recipients and body, then override to send anyway.")
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaint)
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .fill(Palette.dangerSoft)
        )
        .overlay(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .strokeBorder(Palette.danger.opacity(0.4), lineWidth: 1))
    }
}

/// One `LABEL  value` row of a review summary. The value is mono because it is a
/// header being checked character by character — a recipient, a subject.
struct ComposeSummaryRow: View {
    let label: String
    let value: String

    init(_ label: String, _ value: String) {
        self.label = label
        self.value = value
    }

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Text(label)
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
                .textCase(.uppercase)
                .frame(width: 60, alignment: .leading)
            Text(value)
                .font(Typo.mono(12))
                .foregroundStyle(Palette.ink)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}
