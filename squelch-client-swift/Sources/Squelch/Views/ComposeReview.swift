// The send ceremony — the one irreversible action in the app, so it gets two
// phases: edit (⌘Enter → review), then review, which submits once *without*
// override_guard to get the outbound-guard verdict. A clean pass has already
// sent; a 422 shows the redacted guard kinds and demands a distinct override
// (shift+Enter or the danger button). 403 means no write credential.
//
// This is the MODAL composer, and since Wave 5 replies no longer route here —
// they open the reader's inline composer (InlineReply), which runs the same
// ceremony against the same `ComposeSubmit`. What is left to this one is the
// new-message path (`replyToMessageId == nil`), plus the reply shape it still
// supports for any caller that has no thread to open.

import SwiftUI

struct ComposeReview: View {
    @Environment(AppStore.self) private var store
    @FocusState private var focusedField: FocusTarget?

    private enum FocusTarget { case to, subject, body }

    private var compose: ComposeState? { store.compose }
    private var inReview: Bool { compose?.phase == .review }
    private var guarded: Bool { !(compose?.guardKinds.isEmpty ?? true) }

    var body: some View {
        if let compose {
            OverlayScrim(onDismiss: { store.closeCompose() }) {
                ModalCard(width: 620) {
                    HStack(alignment: .firstTextBaseline) {
                        Text(inReview ? "review · confirm send" : "compose")
                            .font(Typo.sectionLabel)
                            .foregroundStyle(Palette.ink)
                            .textCase(.uppercase)
                        Spacer()
                        Text(compose.replyToMessageId != nil ? "reply" : "new message")
                            .font(Typo.micro)
                            .foregroundStyle(Palette.inkFaintest)
                    }

                    if inReview {
                        reviewPane(compose)
                    } else {
                        editPane
                    }

                    if let error = compose.error {
                        Text(error).font(Typo.micro).foregroundStyle(Palette.danger)
                    }

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
                            Button("esc cancel") { store.closeCompose() }
                                .buttonStyle(.glass)
                            Button("review →") { toReview() }
                                .buttonStyle(.glassProminent)
                                .tint(Palette.accent)
                        }
                    }
                }
            }
            .keyContext(.modal)
            .keyBindings(.modal, bindings)
            .onAppear { if !inReview { focusedField = .to } }
        }
    }

    private var editPane: some View {
        VStack(alignment: .leading, spacing: 12) {
            Field(label: "to") {
                TextField("recipient@example.com", text: bind(\.to))
                    .textFieldStyle(.plain)
                    .focused($focusedField, equals: .to)
            }
            Field(label: "subject") {
                // Left blank on a reply the daemon titles from the parent; the
                // placeholder says so, because an empty field otherwise reads as
                // an unset required value.
                TextField(subjectPlaceholder, text: bind(\.subject))
                    .textFieldStyle(.plain)
                    .focused($focusedField, equals: .subject)
            }
            VStack(alignment: .leading, spacing: 5) {
                Text("body").font(Typo.micro).foregroundStyle(Palette.inkFaint)
                TextEditor(text: bind(\.body))
                    .font(.system(size: 13))
                    .scrollContentBackground(.hidden)
                    .focused($focusedField, equals: .body)
                    .frame(height: 170)
                    .padding(8)
                    .background(
                        RoundedRectangle(cornerRadius: 9, style: .continuous)
                            .fill(Palette.canvas.opacity(0.65))
                    )
                    .overlay(
                        RoundedRectangle(cornerRadius: 9, style: .continuous)
                            .strokeBorder(Palette.hairlineStrong, lineWidth: 0.75))
            }
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

            ScrollView {
                Text(compose.body)
                    .font(.system(size: 12.5))
                    .foregroundStyle(Palette.inkDim)
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .frame(maxHeight: 220)
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
                    guard focusedField != .body else { return false }  // let it type a newline
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
            set: { value in patch { $0[keyPath: keyPath] = value } })
    }

    private func patch(_ mutate: (inout ComposeState) -> Void) {
        guard var next = store.compose else { return }
        mutate(&next)
        store.compose = next
    }

    private func toReview() {
        guard let compose = store.compose else { return }
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
/// differently in the modal composer and in the reader's inline one.
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
