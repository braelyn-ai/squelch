// COMPOSE / REVIEW — the send ceremony.
//
// Send is the one irreversible action in the app, so it gets friction
// proportional to that:
//   EDIT   — to / subject / body. ⌘Enter → REVIEW.
//   REVIEW — recipients + body preview + the outbound-guard verdict. We submit
//            ONCE without override_guard to get that verdict:
//              * clean pass → the send actually fired; we close on success.
//              * 422 guard  → show the redacted guard kinds plus a DISTINCT
//                             "override and send" affordance (shift+Enter or
//                             the danger button). A second plain Enter without
//                             a surfaced guard just re-fires.
//   403    — no write credential: render the `squelchd auth --write` hint.
// Esc backs REVIEW → EDIT, and EDIT → cancel.
//
// Ported from squelch-desktop/src/components/ComposeReview.tsx.

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
                TextField("", text: bind(\.subject))
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

    private func reviewPane(_ compose: ComposeState) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            reviewRow("to", compose.to.isEmpty ? "(none)" : compose.to)
            reviewRow("subject", compose.subject.isEmpty ? "(none)" : compose.subject)

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
                VStack(alignment: .leading, spacing: 4) {
                    HStack(spacing: 4) {
                        Text("outbound guard blocked · matched (redacted):")
                            .font(Typo.micro)
                        Text(compose.guardKinds.joined(separator: ", "))
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
    }

    private func reviewRow(_ label: String, _ value: String) -> some View {
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
            // In the body field, plain Enter is a newline; ⌘Enter reviews. The
            // review phase's Enter fires WITHOUT override (phase 1 = verdict).
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
        // Guard against an empty body — nothing to review.
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
        guard let compose = store.compose, !compose.sending else { return }
        patch {
            $0.sending = true
            $0.error = nil
        }
        do {
            try await APIClient.shared.actionSend(
                body: compose.body, replyToMessageId: compose.replyToMessageId,
                to: compose.to.isEmpty ? nil : compose.to,
                subject: compose.subject.isEmpty ? nil : compose.subject,
                overrideGuard: override)
            store.pushToast("sent", .success)
            store.closeCompose()
        } catch let apiError as APIError where apiError.kind == .guardBlocked {
            // Surface the redacted verdict; stay in review, offer an explicit
            // override rather than re-firing the same call.
            patch {
                $0.phase = .review
                $0.guardKinds = apiError.guardKinds ?? []
                $0.sending = false
                $0.error = nil
            }
        } catch let apiError as APIError where apiError.kind == .forbidden {
            patch {
                $0.sending = false
                $0.error = "no write credential — run `squelchd auth --write`"
            }
        } catch {
            patch {
                $0.sending = false
                $0.error = errText(error, "send failed")
            }
        }
    }
}
