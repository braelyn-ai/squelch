// SHARE PASSBAND: address a few friends, edit the mail, send it from YOUR
// mailbox.
//
// IT IS THE COMPOSER, and deliberately so in every way that matters: the same
// `RecipientField` (pills, backspace staging, autocomplete over people the user
// has actually written to), the same subject well, and the same
// `MarkdownTextView` over the same markdown the daemon renders with the
// composer's own renderer. Somebody who has written an email in this app has
// already learned this screen, and an invite IS an email.
//
// THE DRAFT COMES FROM THE DAEMON, not from here. The one line in it that has
// to be true — "I only open about N% of my email" — is a fact only that machine
// can compute, and a client that wrote its own copy would drift from what the
// product says about itself. After that the words are the user's.
//
// THE ONE THING THEY DO NOT EDIT is the invite marker, and only because
// everything it expands to is a fact the daemon guarantees: the link, that
// friend's own code, and the true expiry. Editing it away is a refusal the
// daemon makes and this screen warns about first.

import SwiftUI

struct SharePanel: View {
    /// Read for ONE thing: which surface raised this sheet, for the analytics
    /// property on a successful send. Nothing on this screen renders from it.
    @Environment(AppStore.self) private var store
    @Environment(\.dismiss) private var dismiss

    /// The wire string `RecipientField` parses into pills. Comma-joined, which
    /// is also exactly what the POST wants split.
    @State private var to = ""
    /// The draft: seeded from the daemon on load, and the user's from then on.
    /// Named `draft` rather than `body` because a `View` already has one of
    /// those, and a body that is sometimes markdown and sometimes a view is a
    /// thing nobody should have to hold in their head.
    @State private var subject = ""
    @State private var draft = ""
    @State private var availability: InviteAvailability?
    @State private var loadFailed = false
    @State private var sending = false
    /// Set once a press comes back. Present means the form is done and the
    /// screen is a receipt.
    @State private var outcome: InviteSendResponse?
    @State private var sendError: String?

    /// Named `FocusTarget` and not `Field`, like the composer's: `Field` is the
    /// shared label-plus-well view, and a private enum of that name shadows it
    /// inside this file only, which is the most confusing place for it to.
    @FocusState private var focus: FocusTarget?
    private enum FocusTarget: Hashable { case to, subject, body }

    /// The daemon's ceiling, mirrored so the form can refuse before the round
    /// trip. It is the daemon's number that actually enforces it.
    private static let maxRecipients = 5

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
        VStack(alignment: .leading, spacing: 12) {
            Text(
                "Each friend gets one of these from your own address, with an invite code of "
                    + "their own. Nobody on it can see who else you invited."
            )
            .font(Typo.rowSub)
            .foregroundStyle(Palette.inkDim)
            .fixedSize(horizontal: false, vertical: true)

            RecipientField(text: $to, focus: $focus, field: FocusTarget.to)

            Field(label: "subject") {
                TextField("", text: $subject)
                    .textFieldStyle(.plain)
                    .focused($focus, equals: .subject)
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
                MarkdownTextView(text: $draft)
                    .frame(minHeight: 220, maxHeight: .infinity)
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

            markerHint(availability)

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

    /// What the marker is, and a warning when it has been edited away.
    ///
    /// The daemon refuses a draft without it — an invite mail with no invite in
    /// it is one nobody can act on — so saying so here is what turns that
    /// refusal into something a person can see coming instead of meet.
    @ViewBuilder
    private func markerHint(_ availability: InviteAvailability) -> some View {
        let marker = availability.invite_marker ?? "{{invite}}"
        if draft.contains(marker) {
            Text("\(marker) becomes each friend's own link, code and expiry.")
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
        } else {
            Text(
                "put \(marker) back where the invite should go. without it there is nothing "
                    + "to accept."
            )
            .font(Typo.micro)
            .foregroundStyle(Palette.warn)
            .fixedSize(horizontal: false, vertical: true)
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
            && !subject.trimmed.isEmpty
            && !draft.trimmed.isEmpty
            // The daemon refuses this too; refusing it here is what keeps a
            // press from costing a round trip to be told so.
            && draft.contains(availability?.invite_marker ?? "{{invite}}")
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
            let found = try await APIClient.shared.inviteAvailability()
            availability = found
            // SEEDED ONCE. A reload must never overwrite words somebody has
            // already typed, so the draft is taken only when the fields are
            // still empty.
            if subject.isEmpty { subject = found.subject ?? "" }
            if draft.isEmpty { draft = found.body ?? "" }
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
                to: recipients, subject: subject.trimmed, body: draft)
            outcome = sent
            // ONLY WHEN SOMETHING ACTUALLY WENT. A press where every invite
            // failed is not a share, and an `invite_sent` counting it would
            // make the funnel read best exactly when the feature is most
            // broken. The failures ride along as a property, so a run of them
            // is still visible — under an event whose name is true.
            let delivered = sent.results.filter(\.sent).count
            if delivered > 0 {
                Analytics.capture(
                    "invite_sent",
                    [
                        "count": delivered,
                        "failed": sent.results.count - delivered,
                        // WHICH SURFACE drove this. The whole reason the event
                        // is worth having: "people share" is not actionable,
                        // "the two-week ask is where shares come from" is.
                        "source": store.shareOrigin.rawValue,
                    ])
            }
        } catch {
            // The daemon's own copy where it wrote some, and a plain sentence
            // where it did not. Never the transport error: it says nothing a
            // person can act on.
            sendError = errText(error, "those invites could not be sent.")
        }
    }
}
