// SHARE PASSBAND: type in a few friends, and each of them gets an invite from
// YOUR mailbox, with a code that sets up a mailbox of their own.
//
// THE MAIL IS SHOWN BEFORE IT IS SENT, and that is not a nicety. It goes out
// over the user's own name, from their own address, to somebody who did not ask
// for it; the least this screen can do is show them exactly what that says. The
// preview text comes from the DAEMON, rendered by the same function that
// renders the real thing (see `sharing::get_invites`), so what is on screen
// cannot drift from what is sent.
//
// The recipient field is the composer's own `RecipientField`: pills, backspace
// staging, and autocomplete over people the user has actually written to. An
// invite is a mail like any other, so it is addressed like one.
//
// WHAT THIS SCREEN NEVER DOES: invent a number. The "% of my email" line in the
// preview is either the user's real one or absent entirely, decided by the
// daemon (`sharing::share_stat`) and never by this view.

import SwiftUI

struct SharePanel: View {
    @Environment(\.dismiss) private var dismiss

    /// The wire string `RecipientField` parses into pills. Comma-joined, which
    /// is also exactly what the POST wants split.
    @State private var to = ""
    @State private var note = ""
    @State private var availability: InviteAvailability?
    @State private var loadFailed = false
    @State private var sending = false
    /// Set once a press comes back. Present means the form is done and the
    /// screen is a receipt.
    @State private var outcome: InviteSendResponse?
    @State private var sendError: String?

    @FocusState private var focus: Field?
    private enum Field: Hashable { case to, note }

    /// The daemon's ceiling, mirrored so the form can refuse before the round
    /// trip. It is the daemon's number that actually enforces it.
    private static let maxRecipients = 5
    private static let maxNote = 500

    private var recipients: [String] {
        to.split(separator: ",")
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider().overlay(Palette.hairline)
            content
        }
        .frame(width: 560)
        .frame(maxHeight: 720)
        .background(Palette.canvas)
        .task { await load() }
    }

    // MARK: - chrome

    private var header: some View {
        HStack(alignment: .firstTextBaseline) {
            Text("Share Passband")
                .font(Typo.serif(19))
                .foregroundStyle(Palette.ink)
            Spacer()
            Button("Done") { dismiss() }
                .buttonStyle(.glass)
                .font(.system(size: 12, weight: .medium))
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 16)
    }

    @ViewBuilder
    private var content: some View {
        if let outcome {
            receipt(outcome)
        } else if loadFailed {
            // A daemon that would not answer is NOT a daemon that cannot share.
            // Saying so would be a guess, and the retry is one press.
            message(
                "Passband could not reach your daemon to set this up.",
                detail: "Try again in a moment.")
        } else if let availability, !availability.can_share {
            // The honest version of "you cannot do this", and WHICH honest
            // version depends on why: one of these is nothing the reader can
            // act on, and the other is one command.
            if availability.reason == "no_write_credential" {
                message(
                    "Passband cannot send mail as you yet.",
                    detail:
                        "An invite is sent from your own mailbox, which needs the write "
                        + "credential. Run `squelchd auth --write` on your daemon and come back."
                )
            } else {
                message(
                    "This mailbox cannot send invites.",
                    detail:
                        "Invites are minted by the hosted control plane, so a self-hosted daemon "
                        + "has nowhere to ask. If this is a hosted mailbox, the operator can turn "
                        + "sharing on."
                )
            }
        } else if let availability {
            form(availability)
        } else {
            ProgressView()
                .controlSize(.small)
                .frame(maxWidth: .infinity)
                .padding(.vertical, 48)
        }
    }

    // MARK: - the form

    private func form(_ availability: InviteAvailability) -> some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                Text(
                    "Each friend gets one email from your address, with an invite code that sets "
                        + "up a mailbox of their own. Nobody on it can see who else you invited."
                )
                .font(Typo.rowSub)
                .foregroundStyle(Palette.inkDim)
                .fixedSize(horizontal: false, vertical: true)

                RecipientField(text: $to, focus: $focus, field: Field.to)

                VStack(alignment: .leading, spacing: 5) {
                    FieldLabel("say something (optional)")
                    TextField("", text: $note, axis: .vertical)
                        .textFieldStyle(.plain)
                        .lineLimit(2...5)
                        .focused($focus, equals: .note)
                        .fieldWell()
                    if note.count > Self.maxNote {
                        Text("that is longer than a note (\(note.count) of \(Self.maxNote))")
                            .font(Typo.micro)
                            .foregroundStyle(Palette.danger)
                    }
                }

                preview(availability)

                if let sendError {
                    Text(sendError)
                        .font(Typo.rowSub)
                        .foregroundStyle(Palette.danger)
                        .fixedSize(horizontal: false, vertical: true)
                }

                sendRow
            }
            .padding(20)
        }
    }

    /// The mail, as it will go out. The user's own note sits above the daemon's
    /// text exactly where it will land in the real thing.
    private func preview(_ availability: InviteAvailability) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            FieldLabel("what they get")
            VStack(alignment: .leading, spacing: 10) {
                if !note.trimmed.isEmpty {
                    Text(note.trimmed)
                        .foregroundStyle(Palette.ink)
                }
                Text(availability.preview ?? "")
                    .foregroundStyle(Palette.inkDim)
            }
            .font(.system(size: 12))
            .textSelection(.enabled)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(12)
            .background(
                RoundedRectangle(cornerRadius: 9, style: .continuous)
                    .fill(Palette.readerBackground)
            )
            .overlay(
                RoundedRectangle(cornerRadius: 9, style: .continuous)
                    .strokeBorder(Palette.hairline, lineWidth: 0.75)
            )
            // The code in the preview is not a real one, and somebody will try
            // to use it if nothing here says so.
            Text("the code above is a placeholder; each friend gets their own.")
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
        }
    }

    private var sendRow: some View {
        HStack(spacing: 10) {
            if recipients.count > Self.maxRecipients {
                Text("\(Self.maxRecipients) at a time")
                    .font(Typo.micro)
                    .foregroundStyle(Palette.warn)
            }
            Spacer()
            Button(sending ? "sending" : sendLabel) {
                Task { await send() }
            }
            .buttonStyle(.glassProminent)
            .font(.system(size: 12, weight: .semibold))
            .disabled(!canSend)
        }
    }

    private var sendLabel: String {
        recipients.count > 1 ? "send \(recipients.count) invites" : "send invite"
    }

    private var canSend: Bool {
        !sending
            && !recipients.isEmpty
            && recipients.count <= Self.maxRecipients
            && note.count <= Self.maxNote
    }

    // MARK: - the receipt

    /// What happened, per friend. Deliberately a REPLACEMENT for the form
    /// rather than a banner over it: the codes are spent, and a form still
    /// standing invites a second press that would spend more.
    private func receipt(_ outcome: InviteSendResponse) -> some View {
        VStack(alignment: .leading, spacing: 14) {
            ForEach(outcome.results) { result in
                HStack(alignment: .firstTextBaseline, spacing: 9) {
                    Image(systemName: result.sent ? "checkmark.circle.fill" : "exclamationmark.circle.fill")
                        .foregroundStyle(result.sent ? Palette.positive : Palette.danger)
                        .font(.system(size: 13))
                    VStack(alignment: .leading, spacing: 2) {
                        Text(result.email)
                            .font(Typo.row)
                            .foregroundStyle(Palette.ink)
                        if let error = result.error {
                            Text(error)
                                .font(Typo.rowSub)
                                .foregroundStyle(Palette.inkDim)
                        }
                    }
                    Spacer()
                }
            }
            if let remaining = outcome.remaining {
                Text(
                    remaining == 1
                        ? "1 invite left this month" : "\(remaining) invites left this month"
                )
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaint)
            }
        }
        .padding(20)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func message(_ title: String, detail: String) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title)
                .font(Typo.row)
                .foregroundStyle(Palette.ink)
            Text(detail)
                .font(Typo.rowSub)
                .foregroundStyle(Palette.inkDim)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(20)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    // MARK: - the work

    private func load() async {
        do {
            availability = try await APIClient.shared.inviteAvailability()
            focus = .to
        } catch {
            loadFailed = true
        }
    }

    private func send() async {
        guard canSend else { return }
        sending = true
        sendError = nil
        defer { sending = false }
        do {
            let sent = try await APIClient.shared.sendInvites(
                to: recipients, note: note.trimmed.isEmpty ? nil : note.trimmed)
            outcome = sent
            Analytics.capture(
                "invite_sent",
                ["count": sent.results.filter(\.sent).count, "failed": sent.results.filter { !$0.sent }.count])
        } catch {
            // The daemon's own copy where it wrote some, and a plain sentence
            // where it did not. Never the transport error: it says nothing a
            // person can act on.
            sendError = errText(error, "those invites could not be sent.")
        }
    }
}
