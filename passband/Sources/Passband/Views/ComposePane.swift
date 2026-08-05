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

import AppKit
import SwiftUI

struct ComposePane: View {
    @Environment(AppStore.self) private var store
    @FocusState private var focusedField: FocusTarget?

    private enum FocusTarget { case to, subject }

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
                        editPane
                    }
                    if let error = compose.error {
                        Text(error).font(Typo.micro).foregroundStyle(Palette.danger)
                    }
                }
                .padding(.horizontal, 18)
                .padding(.vertical, 14)
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)

                footer(compose)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .passbandGlass(.pane, cornerRadius: 0, tint: Palette.glassTintStrong)
            .shadow(color: .black.opacity(0.24), radius: 40, x: -14)
            .keyContext(.modal)
            .keyBindings(.modal, bindings)
            .onAppear { if !inReview { focusedField = .to } }
        }
    }

    private func header(_ compose: ComposeState) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Text(inReview ? "review · confirm send" : "compose")
                .font(Typo.sectionLabel)
                .foregroundStyle(Palette.ink)
                .textCase(.uppercase)
            Text(compose.replyToMessageId != nil ? "reply" : "new message")
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
            Spacer()
            HStack(spacing: 4) {
                Kbd("Esc")
                Text("close").font(Typo.micro).foregroundStyle(Palette.inkFaintest)
            }
        }
        .padding(.horizontal, 18)
        .padding(.vertical, 13)
        .overlay(alignment: .bottom) { Hairline() }
    }

    private func footer(_ compose: ComposeState) -> some View {
        HStack(spacing: 8) {
            hint
            Spacer()
            if inReview {
                Button("esc back") { patch { $0.phase = .edit; $0.error = nil } }
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
                // Edit phase only: what goes out is settled by the time review
                // is up, and a switch beside the send button is a switch nobody
                // meant to touch.
                TrackerToggle(on: bindFlag(\.includeTracker))
                Button("esc cancel") { store.closeCompose() }
                    .buttonStyle(.glass)
                Button("review →") { toReview() }
                    .buttonStyle(.glassProminent)
                    .tint(Palette.accent)
            }
        }
        .padding(.horizontal, 18)
        .padding(.vertical, 12)
        .overlay(alignment: .top) { Hairline() }
    }

    private var editPane: some View {
        VStack(alignment: .leading, spacing: 12) {
            RecipientField(text: bind(\.to), focus: $focusedField, field: FocusTarget.to)
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
                    Text("markdown — **bold**, *italic*, `code`, [links](url)")
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkFaintest)
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
        }
    }

    private var isReply: Bool { compose?.replyToMessageId != nil }

    /// Stands in for an empty subject on a reply, in both panes: the daemon titles
    /// it `Re: <parent subject>`, and the real parent subject is not in reach here
    /// (the update carries an LLM summary, not the header).
    private var subjectPlaceholder: String {
        isReply ? ComposeCopy.derivedSubject : "subject"
    }

    private func reviewPane(_ compose: ComposeState) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            ComposeSummaryRow("to", compose.to.isEmpty ? "(none)" : compose.to)
            ComposeSummaryRow(
                "subject",
                compose.subject.isEmpty
                    ? (isReply ? ComposeCopy.derivedSubject : "(none)") : compose.subject)
            // Review states what is about to go out, and an invisible pixel in
            // it is part of that. Only when armed: a row saying "no" on every
            // ordinary send is a row nobody reads.
            if compose.includeTracker && store.trackingAvailable {
                ComposeSummaryRow("tracking", ComposeCopy.trackedSend)
            }

            ScrollView {
                // The scanner's own styling, so review shows the formatting the
                // HTML half will carry — not a second interpretation of it.
                Text(MarkdownStyle.attributed(compose.body))
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .frame(maxHeight: .infinity)
            .padding(10)
            .background(
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .fill(Palette.canvas.opacity(0.6))
            )

            if compose.guardKinds.isEmpty {
                HStack(spacing: 4) {
                    Text("outbound guard: not yet checked ·")
                        .font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                    Kbd("enter")
                    Text("submits for the verdict")
                        .font(Typo.micro).foregroundStyle(Palette.inkFaintest)
                }
            } else {
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

    /// True while the caret sits in the body editor. The body is an NSTextView
    /// now (FocusState cannot see into AppKit), and plain Enter there must stay
    /// a newline.
    private var bodyHasFocus: Bool {
        NSApp.keyWindow?.firstResponder is NSTextView
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
                guard let compose = store.compose, compose.phase == .review,
                    !compose.guardKinds.isEmpty
                else { return false }
                Task { await fire(override: true) }
                return true
            },
        ]
    }

    // MARK: - state helpers

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

    private func toReview() {
        guard let compose = store.compose else { return }
        // Untouched covers the seeded signature: a signature under nothing is
        // not a message, and review must not put it one Enter from going out.
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

    /// The request lives in `ComposeSubmit`; what is left here is this surface's
    /// own reaction to each outcome.
    private func fire(override: Bool) async {
        guard let compose = store.compose, !compose.sending else { return }
        patch {
            $0.sending = true
            $0.error = nil
        }
        switch await ComposeSubmit.fire(compose, override: override) {
        case .sent:
            // The daemon resolved the replied-to update; without this the row
            // sits in its band until the next poll, reading as a no-op. No undo
            // pairs with it — a send is the one irreversible action.
            if let repliedTo = compose.replyToMessageId { store.noteResolved(repliedTo) }
            store.pushToast("sent", .success)
            // The send already deleted the draft; without this the close below
            // would flush it straight back and offer to restore mail that is gone.
            DraftSaver.shared.noteSent(.compose)
            store.closeCompose()
        case .guardBlocked(let kinds):
            // Stay in review with the redacted verdict; the override must be an
            // explicit second act, not a re-fire of the same call.
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
