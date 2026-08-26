// REPLY WHERE YOU READ. A pinned composer under the message stack rather than a
// panel over it: the email you are answering stays on screen, unblurred and
// scrollable, which is the whole point of answering from the reader.
//
// The ceremony is the pane composer's, unchanged and LOCKED: ⌘Enter goes to
// review, Enter submits ONCE WITHOUT override (that call is what fetches the
// outbound-guard verdict), and only a blocked verdict unlocks shift+Enter to send
// anyway. Both composers run it through `ComposeSubmit`, so there is one request
// shape and one error mapping. The body is markdown, live-styled by the same
// MarkdownTextView the pane uses.
//
// THE HEADER LINE IS A DISCLOSURE. Collapsed it says who this reply reaches, in
// one line, because that is all most replies need. Opened it is three editable
// recipient fields — to, cc, bcc — so the answer to "actually, put Dana on bcc"
// is two clicks in the composer you are already in, rather than closing it and
// starting the mail again somewhere that has the fields.
//
// WHICH MEANS THE AUDIENCE CHANGES HANDS. The composer still opens knowing only
// its parent, and the real set is still derived server-side — the parent's
// Reply-To, the room a reply-all widens to. What is new is that the derivation
// is SEEDED into the fields when it lands, and from that moment the fields are
// the answer and the send carries them explicitly (`recipientsStated`). Before
// it lands, and if it never does, the wire carries no recipients at all and the
// daemon derives exactly as it always has — which is why a failed lookup costs a
// preview and never a reply.
//
// The seeded set is also remembered (`seededRecipients`), so the autosave can
// tell "a reply nobody addressed" from "a reply somebody moved to bcc": the
// first must not mint a draft, the second must.
//
// ON A PHONE IT IS THE SAME BAR, pinned by `.safeAreaInset(edge: .bottom)`
// instead of by a VStack — so the keyboard lifts it and insets the mail behind
// it rather than squashing the reader (see ThreadViewer). Two things are fenced.
// The KEY HINT BAR becomes real buttons, and that is not decoration: the
// ceremony is driven ENTIRELY by keys on the Mac, so without them a phone could
// open a reply and have no way on earth to review or send it. And the editor is
// SHORTER, because 150pt of text view above a raised keyboard leaves nothing of
// the email you are answering — which is the whole reason this composer is here
// and not a panel over it.

import SwiftUI

struct InlineReply: View {
    /// The thread's messages, so the draft's `reply_to_message_id` can be resolved
    /// back to the message it answers. Passed in rather than read from the store
    /// because the composer is mounted unconditionally — see ThreadViewer.
    let messages: [ClientMessage]
    /// The thread's subject, for the derived-subject line. Messages carry no
    /// subject of their own on the wire.
    let threadSubject: String
    /// Fired after a send whose echo has already been ingested, so the viewer can
    /// refetch and show the sent copy. Not called when the echo is absent — the
    /// poll catches that up.
    let onEchoed: () -> Void

    @Environment(AppStore.self) private var store
    @FocusState private var focusedField: RecipientSlot?

    /// Whether the recipient fields are open. Per-composer by nature: it resets
    /// when the reply closes, because the next one is a different audience.
    @State private var editingRecipients = false

    /// The daemon's derived recipient set, TAGGED with the key it was fetched
    /// for. The tag is what makes a stale read impossible: this view is
    /// mounted unconditionally, so bare state would survive one composer and
    /// render — for a frame, before the keyed task clears it — under the next.
    /// nil = nothing landed yet; `(key, nil)` = the fetch for that key failed.
    @State private var fetchedRecipients: (key: String, set: ReplyRecipients?)?

    private var compose: ComposeState? { store.inlineReply }
    private var inReview: Bool { compose?.phase == .review }
    private var guarded: Bool { !(compose?.guardKinds.isEmpty ?? true) }
    /// The message being answered. nil = nothing to answer, which renders as no
    /// composer at all.
    private var parent: ClientMessage? {
        guard let id = store.inlineReply?.replyToMessageId else { return nil }
        return messages.first { $0.id == id }
    }
    /// What the daemon will title the reply, mirrored for display.
    private var replySubject: String { ComposeCopy.replySubject(threadSubject) }

    /// The fetch's identity: which parent, in which mode. EVERY reply fetches
    /// now, not just reply-all — the daemon honors the parent's Reply-To on a
    /// plain reply too, so the stored sender the client holds can be the wrong
    /// answer. Mode is part of the key so `r` then Enter on the same message
    /// refetches rather than showing the other mode's set.
    private var recipientsKey: String? {
        guard let compose = store.inlineReply, let parent = compose.replyToMessageId else {
            return nil
        }
        return "\(parent):\(compose.replyAll)"
    }

    /// The fetched set, only if it belongs to the CURRENT key.
    private var recipients: ReplyRecipients? {
        guard let fetchedRecipients, fetchedRecipients.key == recipientsKey else { return nil }
        return fetchedRecipients.set
    }

    /// True once the current key's fetch has come back (with or without a set)
    /// — the difference between "deriving…" and "the daemon will derive it".
    private var recipientsSettled: Bool {
        fetchedRecipients?.key == recipientsKey
    }

    var body: some View {
        if let compose, let parent {
            VStack(alignment: .leading, spacing: 0) {
                VStack(alignment: .leading, spacing: 9) {
                    headerLine(compose, parent: parent)
                    if editingRecipients && !inReview {
                        recipientEditor(compose)
                    }
                    if inReview {
                        reviewPane(compose, parent: parent)
                    } else {
                        editor(compose)
                    }
                    if let error = compose.error {
                        Text(error).font(Typo.micro).foregroundStyle(Palette.danger)
                    }
                }
                .padding(.horizontal, Self.gutter)
                .padding(.top, 12)
                .padding(.bottom, 10)

                #if os(macOS)
                    KeyHintBar(hints: hints)
                #else
                    actionBar(compose)
                #endif
            }
            // The reader's own measure, so the composer sits under the column it
            // answers rather than sprawling the full window width. A phone is
            // narrower than the column will ever be, so there it is inert.
            .frame(maxWidth: ThreadViewer.columnWidth, alignment: .leading)
            .frame(maxWidth: .infinity)
            // A GROUND OF ITS OWN on the phone, and only there. On the Mac this
            // bar sits INSIDE the reader's own material, at the bottom of its
            // stack; as a safe-area inset it floats over scrolling mail, and mail
            // reading through the send button is not a composer.
            #if !os(macOS)
                .passbandGlass(.pane, cornerRadius: 0, tint: Palette.glassTintStrong)
            #endif
            .overlay(alignment: .top) { Hairline() }
            // REGISTRATION ORDER IS LOAD-BEARING. Within a context the LATEST
            // registered set wins, and this set mounts with the composer — after
            // the viewer's — which is the only reason Escape here means "leave the
            // composer" while the viewer's Escape still means "leave the email".
            // Hoisting these onto the always-mounted viewer would register them
            // FIRST and invert that layering: Escape would close the thread out
            // from under an open draft.
            .keyBindings(.thread, bindings)
            // Once per (parent, mode). Keyed, and the fetched value carries its
            // key too, so reopening the composer on another message or in the
            // other mode can never show the last one's addresses — not even for
            // the frame before this task runs.
            .task(id: recipientsKey) { await loadRecipients() }
            // A different message (or the same one in the other mode) is a
            // different audience: the fields close so nobody edits one reply's
            // recipients believing they are another's.
            .onChange(of: recipientsKey) { _, _ in editingRecipients = false }
        }
    }

    /// Ask the daemon who this reply would reach. Best-effort by contract: the
    /// send derives the set again server-side (and, for a reply-all, hard-fails
    /// there if it cannot), so a failure here is a missing preview, never a
    /// blocked reply — which is why it neither surfaces an error nor touches
    /// `compose.error`.
    private func loadRecipients() async {
        guard let key = recipientsKey, let compose = store.inlineReply,
            let parentId = compose.replyToMessageId
        else { return }
        let slot = compose.id
        let fetched = try? await APIClient.shared.replyRecipients(parentId, all: compose.replyAll)
        // The composer may have closed, or moved on, while this was in flight.
        guard recipientsKey == key else { return }
        fetchedRecipients = (key, fetched)
        seed(fetched, into: slot)
    }

    /// HAND THE DERIVED SET TO THE COMPOSER, once, and only while the composer
    /// has not been addressed by anybody yet.
    ///
    /// This is the moment the audience changes hands: before it, the send
    /// carries no recipients and the daemon derives; after it, the fields are
    /// the answer. Seeding what the daemon itself just derived means the two are
    /// the same mail — nobody's reply is quietly re-addressed by the handover.
    ///
    /// Keyed to the composer's identity, like every other write that lands after
    /// an await: the slot may hold a reply to another message by now, and
    /// stamping one message's recipients onto another's draft is the worst
    /// available outcome. `recipientsStated` already being true means either a
    /// restored draft or the sender got here first, and both outrank a
    /// derivation.
    private func seed(_ derived: ReplyRecipients?, into slot: UUID) {
        guard let derived, var next = store.inlineReply, next.id == slot,
            !next.recipientsStated
        else { return }
        let set = Recipients(to: derived.to, cc: derived.cc ?? "")
        next.recipients = set
        // Remembered so the autosave can tell an untouched reply from an
        // addressed one — see `ComposeState.seededRecipients`.
        next.seededRecipients = set
        next.recipientsStated = true
        store.inlineReply = next
    }

    // MARK: - panes

    private func headerLine(_ compose: ComposeState, parent: ClientMessage) -> some View {
        HStack(spacing: 5) {
            // THE WHOLE "replying to <who>" PHRASE IS THE DISCLOSURE, chevron
            // and all: the thing you want to change is the thing you click. In
            // review it stops being a control — that pane's job is stating what
            // goes out, and a live toggle in it is an invitation to edit the
            // mail you are confirming.
            Button {
                editingRecipients.toggle()
                if editingRecipients { focusedField = .to }
            } label: {
                HStack(spacing: 5) {
                    Image(
                        systemName: editingRecipients
                            ? "chevron.down" : "chevron.right"
                    )
                    .font(.system(size: 8, weight: .semibold))
                    .foregroundStyle(Palette.inkFaintest)
                    Text(compose.replyAll ? "replying to all" : "replying to")
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkFaintest)
                    // Addresses and sender strings alike are email-derived:
                    // rendered as Text only, never as markup, and never
                    // interpolated into a localized literal.
                    Text(headerTarget(compose, parent: parent))
                        .font(.system(size: 11, weight: .medium))
                        .foregroundStyle(Palette.inkDim)
                        .lineLimit(1)
                        .truncationMode(.tail)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .disabled(inReview)
            .accessibilityLabel(editingRecipients ? "hide recipients" : "edit recipients")
            Text("·").foregroundStyle(Palette.inkFaintest)
            Text(replySubject)
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
                .lineLimit(1)
                .truncationMode(.tail)
            Spacer(minLength: 8)
            // Edit phase only, same as the pane composer: review is for reading
            // what is about to go out, not for changing it.
            if !inReview { TrackerToggle(on: bindFlag(\.includeTracker)) }
            if compose.sending {
                Text("sending…")
                    .font(Typo.micro)
                    .foregroundStyle(Palette.accent)
            }
        }
    }

    /// Who the header names. A plain reply names the parent's sender; a
    /// reply-all names the fetched set — and until that lands (or when it never
    /// does) it says the derivation is pending rather than naming the sender,
    /// who is NOT certainly in the set: a mailing list's Reply-To routes the
    /// mail somewhere the sender's own address never appears.
    private func headerTarget(_ compose: ComposeState, parent: ClientMessage) -> String {
        // Once the fields hold the answer they ARE the answer, reply-all or
        // not: somebody who just moved a name to bcc has to see the header
        // agree with what they did.
        if compose.recipientsStated, let summary = recipientSummary(compose) { return summary }
        let sender = SenderCache.resolved(parent.senderString).displayName
        guard compose.replyAll else { return sender }
        return recipientsSettled ? "recipients derived at send" : "deriving recipients…"
    }

    /// "alice@example.com +3 more" — the header is one line above the mail, so a
    /// twelve-person thread has to collapse into a count rather than push the
    /// composer around. The full set is in the fields below, and in review.
    ///
    /// COUNTS BLIND COPIES IN THE TOTAL but never names one first: the summary
    /// leads with the visible audience, because "who is this to" is the question
    /// it answers. The bcc row states itself, in the fields and in review.
    private func recipientSummary(_ compose: ComposeState) -> String? {
        let r = compose.recipients
        let visible = r.tokens(.to) + r.tokens(.cc)
        let total = visible.count + r.count(.bcc)
        guard let first = visible.first ?? r.tokens(.bcc).first else { return nil }
        guard total > 1 else { return first }
        return "\(first) +\(total - 1) more"
    }

    /// THE THREE RECIPIENT FIELDS, opened from the header line.
    ///
    /// Not shown until the derivation has landed, and that is a correctness rule
    /// rather than a loading state: editing empty fields beforehand would make
    /// this composer state an audience it never learned, and on a reply-all the
    /// mail would go to one person instead of the room. The wait is one metadata
    /// fetch, and it started when the composer opened.
    @ViewBuilder
    private func recipientEditor(_ compose: ComposeState) -> some View {
        if compose.recipientsStated {
            VStack(alignment: .leading, spacing: 8) {
                ForEach(RecipientSlot.allCases, id: \.self) { slot in
                    RecipientField(
                        recipients: recipientsBinding, slot: slot, focus: $focusedField,
                        field: slot,
                        // The one field with nothing to seed it says why it is
                        // empty, rather than reading as a value that got lost.
                        placeholder: slot == .bcc ? "nobody is blind-copied" : nil)
                }
            }
            .padding(.bottom, 2)
        } else {
            Text("deriving recipients…")
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
        }
    }

    private func editor(_ compose: ComposeState) -> some View {
        // autofocus is the affordance: `r` must land the cursor in the body, or
        // the composer is a box you have to go click. It lives on the EDITOR,
        // not the bar: the editor also mounts on the way BACK from review (the
        // bar never left), so Esc out of review would otherwise drop the cursor
        // and hand every letter you typed next to the reader's verbs.
        MarkdownTextView(text: bind(\.body), autofocus: true, disabled: compose.sending)
            .frame(height: Self.editorHeight)
            .padding(8)
            .background(
                RoundedRectangle(cornerRadius: 9, style: .continuous)
                    .fill(Palette.canvas.opacity(0.65))
            )
            .overlay(
                RoundedRectangle(cornerRadius: 9, style: .continuous)
                    .strokeBorder(Palette.hairlineStrong, lineWidth: 0.75))
    }

    /// Read-only, because review is for reading: the recipient and subject the
    /// daemon derived, the body as it will go out, and the guard's verdict once
    /// there is one.
    private func reviewPane(_ compose: ComposeState, parent: ClientMessage) -> some View {
        VStack(alignment: .leading, spacing: 9) {
            // Review must show the REAL derived set, not the guess a client
            // would make by scraping headers — this is the last screen before
            // the mail goes out, and the daemon honors Reply-To even on a plain
            // reply, so the stored sender can be the wrong answer. Until the
            // lookup lands (or when it never does) a plain reply falls back to
            // the stored sender and a reply-all names the daemon as the
            // authority; the send still goes, and the daemon fails a reply-all
            // there if it cannot derive the set. Recipient rows are CAPPED —
            // a thirty-person Cc must not grow the pinned composer into the
            // mail it sits under.
            if compose.recipientsStated {
                // The fields are the answer, so review reads them — including
                // anything moved between them since the derivation landed.
                ComposeSummaryRow("to", compose.to.trimmed.isEmpty ? "(none)" : compose.to)
                    .lineLimit(3)
                if !compose.cc.trimmed.isEmpty {
                    ComposeSummaryRow("cc", compose.cc).lineLimit(3)
                }
                // THE ROW REVIEW EXISTS FOR. Everything else on this pane is
                // also visible in the mail once it lands; a blind copy is
                // visible nowhere, to nobody, ever again. This is the last
                // screen that can say who it went to.
                if !compose.bcc.trimmed.isEmpty {
                    ComposeSummaryRow("bcc", compose.bcc).lineLimit(3)
                }
            } else if let recipients, !recipients.to.trimmed.isEmpty {
                ComposeSummaryRow("to", recipients.to).lineLimit(3)
                if let cc = recipients.cc, !cc.trimmed.isEmpty {
                    ComposeSummaryRow("cc", cc).lineLimit(3)
                }
            } else if compose.replyAll {
                ComposeSummaryRow("to", ComposeCopy.derivedRecipients)
            } else {
                ComposeSummaryRow("to", parent.from_addr)
            }
            ComposeSummaryRow("subject", replySubject)
            // Same as the pane composer: review states everything about to go
            // out, and the pixel is the one part the body cannot show.
            if compose.includeTracker && store.trackingAvailable {
                ComposeSummaryRow("tracking", ComposeCopy.trackedSend)
            }

            ScrollView {
                // Same styling as the live editor, so review is the send's
                // formatting, not a second interpretation of it.
                Text(MarkdownStyle.attributed(compose.body))
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .frame(maxHeight: Self.editorHeight)
            .padding(10)
            .background(
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .fill(Palette.canvas.opacity(0.6))
            )

            if guarded { GuardVerdictBox(kinds: compose.guardKinds) }
        }
    }

    /// The composer's own inset. The Mac's is the reader column's 22; a phone is
    /// narrower and the mail beside it is inset 18, so the reply lines up with
    /// the message it answers rather than sitting proud of it.
    #if os(macOS)
        private static let gutter: CGFloat = 22
        private static let editorHeight: CGFloat = 150
    #else
        private static let gutter: CGFloat = 18
        /// Short enough that a couple of lines of the email survive above a
        /// raised keyboard. The editor scrolls itself past that.
        private static let editorHeight: CGFloat = 104
    #endif

    #if os(macOS)
        private var hints: [KeyHint] {
            if inReview {
                var hints = [KeyHint("enter", "send")]
                if guarded { hints.append(KeyHint("shift+enter", "send anyway")) }
                hints.append(KeyHint("esc", "back"))
                return hints
            }
            return [KeyHint("⌘enter", "review"), KeyHint("esc", "dismiss")]
        }
    #endif

    #if !os(macOS)
        /// THE PHONE'S HALF OF THE CEREMONY. Same two phases, same single
        /// `fire(override:)`, same rule that a blocked verdict is the ONLY thing
        /// that unlocks an override: what changes is that a thumb presses them
        /// instead of ⌘Enter, Enter and shift+Enter. The layout mirrors the pane
        /// composer's footer so a reply reads the same wherever it started.
        @ViewBuilder
        private func actionBar(_ compose: ComposeState) -> some View {
            HStack(spacing: 8) {
                Spacer()
                if inReview {
                    Button(ComposeLabels.back) { patch { $0.phase = .edit; $0.error = nil } }
                        .buttonStyle(.glass)
                        .disabled(compose.sending)
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
                    Button(ComposeLabels.dismiss) { store.closeInlineReply() }
                        .buttonStyle(.glass)
                    Button("review →") { toReview() }
                        .buttonStyle(.glassProminent)
                        .tint(Palette.accent)
                }
            }
            .padding(.horizontal, Self.gutter)
            .padding(.bottom, 10)
        }
    #endif

    // MARK: - keymap

    private var bindings: [KeyBinding] {
        [
            // Escape LAYERS: review → edit, edit → close the composer, and only
            // the NEXT press reaches the viewer's Escape and closes the email.
            KeyBinding(
                "Escape", inReview ? "back to edit" : "dismiss reply", allowInInput: true
            ) {
                guard let compose = store.inlineReply else { return }
                if compose.phase == .review {
                    patch { $0.phase = .edit; $0.error = nil }
                } else {
                    store.closeInlineReply()
                }
            },
            // In the body plain Enter is a NEWLINE, so this declines in edit and
            // the keystroke falls through to the text view. In review it fires
            // without override — that call is the verdict.
            KeyBinding(declining: "Enter", inReview ? "send" : "newline", allowInInput: true) {
                guard let compose = store.inlineReply, compose.phase == .review
                else { return false }
                Task { await fire(override: false) }
                return true
            },
            KeyBinding("Enter", "review", meta: true, allowInInput: true) {
                if store.inlineReply?.phase == .edit { toReview() }
            },
            // Explicit override: review phase, blocked verdict, nothing else.
            // Declines otherwise so shift+Enter still types a newline while
            // composing.
            KeyBinding(declining: "shift+Enter", "send anyway", allowInInput: true) {
                guard let compose = store.inlineReply, compose.phase == .review,
                    !compose.guardKinds.isEmpty
                else { return false }
                Task { await fire(override: true) }
                return true
            },
        ] + reviewGuards
    }

    /// REVIEW PHASE ONLY: the reader's own resolving verbs, swallowed.
    ///
    /// While review is up nothing is focused, so `isEditing` stops suppressing the
    /// viewer's single-letter keys — and e/d (done + next) and h/l (queue nav)
    /// would navigate away from a draft that is one keystroke from going out, with
    /// no undo for a lost reply. These decline outside review, so the viewer keeps
    /// them everywhere else, including while the body has focus and eats them
    /// anyway. j/k (scrolling) and the modal verbs are left alone: harmless or
    /// recoverable.
    private var reviewGuards: [KeyBinding] {
        ["e", "d", "h", "l"].map { key in
            KeyBinding(declining: key, "held — reviewing a reply") {
                store.inlineReply?.phase == .review
            }
        }
    }

    // MARK: - state helpers

    private func bind(_ keyPath: WritableKeyPath<ComposeState, String>) -> Binding<String> {
        Binding(
            get: { store.inlineReply?[keyPath: keyPath] ?? "" },
            set: { value in
                // The autosave's one hook for this composer — the body is the only
                // field it has, and it is bound through here.
                guard store.inlineReply?[keyPath: keyPath] != value else { return }
                patch { $0[keyPath: keyPath] = value }
                DraftSaver.shared.noteChange(.inlineReply)
            })
    }

    /// The three recipient fields as one binding. Writes go through
    /// `stateRecipients` — touching a recipient field is the sender taking the
    /// audience over from the daemon — and arm the autosave like any other
    /// edit: a bcc added and then abandoned has to come back.
    private var recipientsBinding: Binding<Recipients> {
        Binding(
            get: { store.inlineReply?.recipients ?? Recipients() },
            set: { value in
                guard store.inlineReply?.recipients != value else { return }
                patch { $0.stateRecipients(value) }
                DraftSaver.shared.noteChange(.inlineReply)
            })
    }

    /// Same shape as `bind`, minus the autosave: a draft records what was
    /// written, not how the next send is addressed.
    private func bindFlag(_ keyPath: WritableKeyPath<ComposeState, Bool>) -> Binding<Bool> {
        Binding(
            get: { store.inlineReply?[keyPath: keyPath] ?? false },
            set: { value in patch { $0[keyPath: keyPath] = value } })
    }

    private func patch(_ mutate: (inout ComposeState) -> Void) {
        guard var next = store.inlineReply else { return }
        mutate(&next)
        store.inlineReply = next
    }

    /// Patch the slot ONLY IF it still holds the composer `id` names. The pane
    /// composer keeps the identical pair for the identical reason: the plain
    /// `patch` above is safe only because its callers run synchronously off a
    /// keystroke, while a send's continuation resumes into whatever the slot
    /// holds by then — which may be a reply to another message entirely. See
    /// `ComposeState.id`.
    private func patch(_ id: UUID, _ mutate: (inout ComposeState) -> Void) {
        guard var next = store.inlineReply, next.id == id else { return }
        mutate(&next)
        store.inlineReply = next
    }

    /// Same empty-body guard as the modal composer: an accidental ⌘Enter must not
    /// put a blank reply one keystroke away from going out.
    private func toReview() {
        guard let compose = store.inlineReply else { return }
        // Same seed rule as the pane: an untouched signature is an empty body.
        guard !Prefs.shared.isBodyUntouched(compose.body) else {
            patch { $0.error = "body is empty" }
            return
        }
        patch {
            $0.phase = .review
            $0.error = nil
            $0.guardKinds = []
        }
    }

    /// EVERY WRITE BELOW IS KEYED TO `slot`, the composer this send belongs to.
    /// `store.inlineReply` is a slot, not an object: while the await is out the
    /// sender can Escape (which flushes the draft and empties the slot) and open
    /// a reply on another message into it, and an unkeyed continuation would
    /// then land on that draft — `noteSent` clearing its touched mark, so the
    /// close's flush refuses to save, so everything typed into it is gone. See
    /// `ComposeState.id`.
    private func fire(override: Bool) async {
        guard let compose = store.inlineReply, !compose.sending else { return }
        let slot = compose.id
        patch(slot) {
            $0.sending = true
            $0.error = nil
        }
        switch await ComposeSubmit.fire(compose, override: override) {
        case .sent(let result):
            // The daemon resolved the replied-to update; without this the row sits
            // in whatever list is mounted behind the reader until the next poll,
            // reading as a no-op. No undo pairs with a send.
            //
            // Not keyed to the slot, and deliberately: this is a fact about the
            // MAIL rather than about the composer, and it stands whoever holds
            // the slot by now. Same for the toast.
            if let repliedTo = compose.replyToMessageId { store.noteResolved(repliedTo) }
            store.pushToast("reply sent", .success)
            // An echo means the sent copy is ALREADY in the local store, so a
            // refetch shows the reply in the thread it belongs to. Without one the
            // ingest has not caught up and there is nothing to fetch — the poll
            // gets there.
            let echoed = result.echo_message_id != nil
            // The slot half. The send already deleted the draft it carried, so
            // without `noteSent` the close would flush it back and leave a reply
            // that went out sitting there to be restored — but only for THIS
            // composer. If the slot moved on, both of these belong to another
            // draft and the right amount of work to do is none.
            if store.inlineReply?.id == slot {
                DraftSaver.shared.noteSent(.inlineReply)
                store.closeInlineReply()
            }
            if echoed { onEchoed() }
        case .guardBlocked(let kinds):
            // Stay in review with the verdict; the override is a separate act.
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
