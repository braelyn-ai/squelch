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
// Body only, on purpose: the request carries `reply_to_message_id` and the body,
// and the daemon derives the recipient and `Re: <subject>` from the parent. The
// header line here is INFORMATIONAL — it states what the daemon will do, it does
// not send anything.

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

    var body: some View {
        if let compose, let parent {
            VStack(alignment: .leading, spacing: 0) {
                VStack(alignment: .leading, spacing: 9) {
                    headerLine(compose, parent: parent)
                    if inReview {
                        reviewPane(compose, parent: parent)
                    } else {
                        editor(compose)
                    }
                    if let error = compose.error {
                        Text(error).font(Typo.micro).foregroundStyle(Palette.danger)
                    }
                }
                .padding(.horizontal, 22)
                .padding(.top, 12)
                .padding(.bottom, 10)

                KeyHintBar(hints: hints)
            }
            // The reader's own measure, so the composer sits under the column it
            // answers rather than sprawling the full window width.
            .frame(maxWidth: ThreadViewer.columnWidth, alignment: .leading)
            .frame(maxWidth: .infinity)
            .overlay(alignment: .top) { Hairline() }
            // REGISTRATION ORDER IS LOAD-BEARING. Within a context the LATEST
            // registered set wins, and this set mounts with the composer — after
            // the viewer's — which is the only reason Escape here means "leave the
            // composer" while the viewer's Escape still means "leave the email".
            // Hoisting these onto the always-mounted viewer would register them
            // FIRST and invert that layering: Escape would close the thread out
            // from under an open draft.
            .keyBindings(.thread, bindings)
        }
    }

    // MARK: - panes

    private func headerLine(_ compose: ComposeState, parent: ClientMessage) -> some View {
        HStack(spacing: 5) {
            Text("replying to")
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
            // Sender strings are email-derived: rendered as Text only, never as
            // markup, and never interpolated into a localized literal.
            Text(SenderCache.resolved(parent.senderString).displayName)
                .font(.system(size: 11, weight: .medium))
                .foregroundStyle(Palette.inkDim)
                .lineLimit(1)
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

    private func editor(_ compose: ComposeState) -> some View {
        // autofocus is the affordance: `r` must land the cursor in the body, or
        // the composer is a box you have to go click. It lives on the EDITOR,
        // not the bar: the editor also mounts on the way BACK from review (the
        // bar never left), so Esc out of review would otherwise drop the cursor
        // and hand every letter you typed next to the reader's verbs.
        MarkdownTextView(text: bind(\.body), autofocus: true, disabled: compose.sending)
            .frame(height: 150)
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
            ComposeSummaryRow("to", parent.from_addr)
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
            .frame(maxHeight: 150)
            .padding(10)
            .background(
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .fill(Palette.canvas.opacity(0.6))
            )

            if guarded { GuardVerdictBox(kinds: compose.guardKinds) }
        }
    }

    private var hints: [KeyHint] {
        if inReview {
            var hints = [KeyHint("enter", "send")]
            if guarded { hints.append(KeyHint("shift+enter", "send anyway")) }
            hints.append(KeyHint("esc", "back"))
            return hints
        }
        return [KeyHint("⌘enter", "review"), KeyHint("esc", "dismiss")]
    }

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

    /// Same empty-body guard as the modal composer: an accidental ⌘Enter must not
    /// put a blank reply one keystroke away from going out.
    private func toReview() {
        guard let compose = store.inlineReply else { return }
        guard !compose.body.trimmed.isEmpty else {
            patch { $0.error = "body is empty" }
            return
        }
        patch {
            $0.phase = .review
            $0.error = nil
            $0.guardKinds = []
        }
    }

    private func fire(override: Bool) async {
        guard let compose = store.inlineReply, !compose.sending else { return }
        patch {
            $0.sending = true
            $0.error = nil
        }
        switch await ComposeSubmit.fire(compose, override: override) {
        case .sent(let result):
            // The daemon resolved the replied-to update; without this the row sits
            // in whatever list is mounted behind the reader until the next poll,
            // reading as a no-op. No undo pairs with a send.
            if let repliedTo = compose.replyToMessageId { store.noteResolved(repliedTo) }
            store.pushToast("reply sent", .success)
            // An echo means the sent copy is ALREADY in the local store, so a
            // refetch shows the reply in the thread it belongs to. Without one the
            // ingest has not caught up and there is nothing to fetch — the poll
            // gets there.
            let echoed = result.echo_message_id != nil
            // The send already deleted the draft; the close below must not flush it
            // back and leave a reply that went out sitting there to be restored.
            DraftSaver.shared.noteSent(.inlineReply)
            store.closeInlineReply()
            if echoed { onEchoed() }
        case .guardBlocked(let kinds):
            // Stay in review with the verdict; the override is a separate act.
            patch {
                $0.phase = .review
                $0.guardKinds = kinds
                $0.sending = false
                $0.error = nil
            }
        case .forbidden:
            patch {
                $0.sending = false
                $0.error = ComposeCopy.noWriteCredential
            }
        case .failure(let text):
            patch {
                $0.sending = false
                $0.error = text
            }
        }
    }
}
