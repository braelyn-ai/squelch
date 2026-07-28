// ⌘K "ASK YOUR INBOX" — the embedded assistant's command bar. A top-anchored
// overlay: type a question, get a cited answer, jump to a thread if you want
// the detail. The whole loop runs on this machine with the user's own key
// (BYOK); the squelch server never sees it.
//
// GLASS: the bar and its answer share one glassEffectID inside a
// GlassEffectContainer, so the material GROWS downward as the answer arrives
// rather than a second panel appearing beneath a first. That continuity is the
// whole reason to use the real material here.
//
// Ported from squelch-desktop/src/components/AskBar.tsx.

import SwiftUI

struct AskBar: View {
    let onClose: () -> Void

    @Environment(AppStore.self) private var store
    @State private var question = ""
    @State private var phase: Phase = .idle
    @State private var answer: AssistantAnswer?
    @State private var error: String?
    @Namespace private var askGlass
    @FocusState private var focused: Bool

    private enum Phase { case idle, loading, done, failed }

    var body: some View {
        OverlayScrim(alignment: .top, topInset: 96, onDismiss: onClose) {
            GlassEffectContainer(spacing: 10) {
                VStack(alignment: .leading, spacing: 0) {
                    header
                    inputRow
                    body_
                }
                .frame(width: 620)
                .squelchGlass(
                    .pane, cornerRadius: 20, tint: Palette.glassTintStrong,
                    id: "askbar", in: askGlass)
                .shadow(color: .black.opacity(0.34), radius: 50, y: 22)
            }
        }
        .keyContext(.modal)
        // Enter submits from the field directly (below), not via the keymap, so
        // it never collides with list keys.
        .keyBindings(.modal, [
            KeyBinding("Escape", "close", allowInInput: true) { onClose() }
        ])
        .onAppear { focused = true }
        .animation(.smooth(duration: 0.25), value: phase)
    }

    private var header: some View {
        HStack(spacing: 7) {
            Image(systemName: "sparkles")
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(Palette.accent)
            Text("ask your inbox")
                .font(Typo.sectionLabel)
                .foregroundStyle(Palette.inkFaint)
                .textCase(.uppercase)
            Spacer()
            Kbd("esc")
        }
        .padding(.horizontal, 16)
        .padding(.top, 13)
        .padding(.bottom, 8)
    }

    private var inputRow: some View {
        HStack(spacing: 8) {
            TextField("e.g. what did I say I'd send Dan?", text: $question)
                .textFieldStyle(.plain)
                .font(.system(size: 16))
                .foregroundStyle(Palette.ink)
                .focused($focused)
                .disabled(phase == .loading)
                .onSubmit { Task { await submit() } }
            Image(systemName: "return")
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(
                    question.trimmed.isEmpty ? Palette.inkFaintest : Palette.accent)
        }
        .padding(.horizontal, 16)
        .padding(.bottom, 13)
    }

    @ViewBuilder
    private var body_: some View {
        switch phase {
        case .loading:
            HStack(spacing: 8) {
                ProgressView().controlSize(.mini)
                Text("searching your inbox…")
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.inkFaint)
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 14)

        case .failed:
            if let error {
                Text(error)
                    .font(Typo.rowSub)
                    .foregroundStyle(Palette.danger)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 16)
                    .padding(.bottom, 14)
            }

        case .done:
            if let answer {
                VStack(alignment: .leading, spacing: 10) {
                    Divider().overlay(Palette.hairline)
                    ScrollView {
                        Text(answer.text)
                            .font(.system(size: 14))
                            .foregroundStyle(Palette.ink)
                            .textSelection(.enabled)
                            .fixedSize(horizontal: false, vertical: true)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                    .frame(maxHeight: 260)

                    if !answer.citations.isEmpty {
                        VStack(alignment: .leading, spacing: 4) {
                            Text("sources")
                                .font(Typo.micro)
                                .foregroundStyle(Palette.inkFaintest)
                                .textCase(.uppercase)
                            ForEach(answer.citations) { citation in
                                Button {
                                    store.openThread(citation.threadId)
                                    onClose()
                                } label: {
                                    HStack(spacing: 7) {
                                        Avatar(sender: citation.sender, size: 16)
                                        Text(citation.sender)
                                            .font(Typo.micro)
                                            .foregroundStyle(Palette.inkDim)
                                            .lineLimit(1)
                                        Text(citation.subject)
                                            .font(Typo.micro)
                                            .foregroundStyle(Palette.inkFaintest)
                                            .lineLimit(1)
                                        Spacer(minLength: 4)
                                        Image(systemName: "arrow.up.right")
                                            .font(.system(size: 9, weight: .semibold))
                                            .foregroundStyle(Palette.accent)
                                    }
                                    .padding(.horizontal, 9)
                                    .padding(.vertical, 5)
                                    .contentShape(Rectangle())
                                }
                                .buttonStyle(RecordRowStyle())
                                .help("Open this thread")
                            }
                        }
                    }
                }
                .padding(.horizontal, 16)
                .padding(.bottom, 14)
            }

        case .idle:
            EmptyView()
        }
    }

    private func submit() async {
        let q = question.trimmed
        guard !q.isEmpty, phase != .loading else { return }
        phase = .loading
        error = nil
        answer = nil
        do {
            answer = try await Assistant.ask(q)
            phase = .done
        } catch {
            self.error = errText(error, "something went wrong")
            phase = .failed
        }
    }
}
