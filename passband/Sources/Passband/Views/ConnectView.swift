// First-run connect screen. Two ways in, one destination: a bearer token in the
// OS keychain, proved against /client/stats before it is stored.
//
// PAIRING is the default. `squelchd pair` prints a code, this screen trades it
// for a token minted for THIS Mac alone — one that shows up by name in
// `squelchd token list` and dies on `squelchd token revoke`. Pasting a raw
// token stays available as the advanced path, because a self-hosted daemon's
// SQUELCH_API_TOKEN is still a first-class way in and always will be.
//
// Neither the code nor the token is ever logged, echoed into an error, or held
// anywhere but this view's own state on its way to the keychain.

import SwiftUI

/// Which way in the screen is showing.
private enum ConnectMode: Hashable { case pair, token }

/// Field focus, so Enter walks the form instead of dead-ending. `submit` is the
/// button itself: a deep link fills the form and lands here, because pairing is
/// never something a link does on its own.
private enum ConnectField: Hashable { case url, code, device, token, submit }

struct ConnectView: View {
    @Environment(AppStore.self) private var store

    @State private var mode: ConnectMode = .pair
    @State private var url = "http://127.0.0.1:8848"
    @State private var code = ""
    @State private var deviceName = Pairing.defaultDeviceName()
    @State private var token = ""
    /// A claim is in flight. Separate from `connStatus`, which only starts
    /// moving once pairing has produced a token to test.
    @State private var claiming = false
    /// The pairing failure, which is this view's own — the store never sees the
    /// code or its rejection.
    @State private var pairError: String?
    /// A token a claim already minted that `connect` has not accepted yet. Held
    /// because a retry must re-run the PROBE, not the claim: claiming again
    /// spends another of the code's attempts and mints a second token nobody
    /// holds, which only the operator can clear with `squelchd token revoke`.
    /// Dropped once connect succeeds, and whenever the user edits the url or
    /// the code, since a token one daemon minted is nothing to another.
    @State private var heldToken: String?
    /// The host a deep link named, when the link did not name this machine's
    /// own daemon. Shown above the form: pointing this app at someone else's
    /// server has to be a visible act, not a silently rewritten field.
    @State private var linkHost: String?
    /// A deep link filled the form and stopped. Rings the button that is
    /// waiting for the press the link will never make for the user.
    @State private var linkArmed = false
    @FocusState private var focus: ConnectField?

    private var busy: Bool { claiming || store.connStatus == .connecting }

    /// One error line, whichever half produced it. A pairing failure wins: it is
    /// the more recent thing the user did.
    private var errorText: String? { pairError ?? store.connError }

    private var canSubmit: Bool {
        guard !busy, !url.trimmed.isEmpty else { return false }
        switch mode {
        // A held token is a claim already paid for, and the code that bought it
        // is spent and cleared, so the code field is not what gates the retry.
        case .pair:
            return heldToken != nil
                || (Pairing.looksComplete(code) && !deviceName.trimmed.isEmpty)
        case .token: return !token.trimmed.isEmpty
        }
    }

    var body: some View {
        ZStack {
            // A fresh download lands here with no daemon and no idea what one
            // is, so the gate teaches: getting-started beside the form when the
            // window is wide enough, the form alone when it is not.
            ViewThatFits(in: .horizontal) {
                HStack(spacing: 20) {
                    GettingStartedPane()
                    connectCard
                }
                connectCard
            }
        }
        // The phone's screen margin. On the Mac the card is a fixed measure
        // floating in a large window and needs none.
        #if !os(macOS)
            .padding(.horizontal, 16)
        #endif
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        // A link can arrive before this gate mounts (the app was launched by
        // one) or while it is up, so both entry points are covered.
        .task { applyPairLink(store.pairLink) }
        .onChange(of: store.pairLink) { _, link in applyPairLink(link) }
    }

    private var connectCard: some View {
            VStack(spacing: 0) {
                header
                linkNotice
                GlassSegmented(
                    options: [(ConnectMode.pair, "pair with code"), (.token, "api token")],
                    selection: modeBinding)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.bottom, 16)

                switch mode {
                case .pair: pairForm
                case .token: tokenForm
                }

                if let errorText {
                    Label(errorText, systemImage: "exclamationmark.triangle.fill")
                        .font(.system(size: 12))
                        .foregroundStyle(Palette.danger)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.top, 12)
                }

                Button {
                    submit()
                } label: {
                    Text(buttonLabel)
                        .font(.system(size: 13, weight: .semibold))
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 5)
                }
                .buttonStyle(.glassProminent)
                .tint(Palette.accent)
                .disabled(!canSubmit)
                .focused($focus, equals: .submit)
                // A link fills the form and stops. The ring is where the one
                // deliberate press has to land for anything to be claimed.
                .overlay {
                    if linkArmed {
                        RoundedRectangle(cornerRadius: 12, style: .continuous)
                            .strokeBorder(Palette.accent, lineWidth: 2)
                            .padding(-4)
                            .allowsHitTesting(false)
                    }
                }
                .padding(.top, 18)

                Text(footnote)
                    .font(.system(size: 11))
                    .foregroundStyle(Palette.inkFaintest)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.top, 14)
            }
            .padding(30)
            // A window can always be 440pt wide; a phone cannot. Same intended
            // measure either way — the Mac states it, the phone treats it as a
            // ceiling and takes whatever the screen leaves (see body's margin).
            #if os(macOS)
                .frame(width: 440)
            #else
                .frame(maxWidth: 440)
            #endif
            .passbandGlass(.pane, cornerRadius: 24, tint: Palette.glassTintStrong)
            .shadow(color: .black.opacity(0.3), radius: 50, y: 24)
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("passband")
                .font(Typo.serif(40, weight: .medium))
                .foregroundStyle(Palette.ink)
            Text("connect to your human door")
                .font(.system(size: 13))
                .foregroundStyle(Palette.inkFaint)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.bottom, 22)
    }

    /// Names the server a deep link chose, whenever that server is not this
    /// machine's own daemon. The code is a credential and this form is where it
    /// gets typed, so where the link intends to send it is said out loud first.
    @ViewBuilder private var linkNotice: some View {
        if let linkHost {
            HStack(alignment: .top, spacing: 9) {
                Image(systemName: "exclamationmark.shield.fill")
                    .font(.system(size: 13, weight: .semibold))
                VStack(alignment: .leading, spacing: 3) {
                    Text("This link wants to pair with \(linkHost)")
                        .font(.system(size: 12, weight: .semibold))
                    Text(
                        "That is not this Mac's own daemon. Your code goes to whatever server this form names, so only continue if you know that one."
                    )
                    .font(.system(size: 11))
                    .fixedSize(horizontal: false, vertical: true)
                }
            }
            .foregroundStyle(Palette.warn)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 12)
            .padding(.vertical, 10)
            .background(
                RoundedRectangle(cornerRadius: 11, style: .continuous)
                    .fill(Palette.warnSoft)
            )
            .padding(.bottom, 16)
        }
    }

    // MARK: - forms

    private var pairForm: some View {
        VStack(alignment: .leading, spacing: 14) {
            Field(label: "server url") {
                TextField("http://127.0.0.1:8848", text: urlBinding)
                    .textFieldStyle(.plain)
                    .textContentType(.URL)
                    .autocorrectionDisabled()
                    .focused($focus, equals: .url)
                    .onSubmit { focus = .code }
            }
            Field(label: "pairing code") {
                TextField("XXXX-XXXX", text: codeBinding)
                    .textFieldStyle(.plain)
                    // Monospaced so a code read off a terminal lines up with
                    // what the terminal showed, character for character.
                    .font(Typo.mono(13))
                    .autocorrectionDisabled()
                    .focused($focus, equals: .code)
                    .onSubmit { submit() }
            }
            Field(label: "device name") {
                TextField("this Mac", text: $deviceName)
                    .textFieldStyle(.plain)
                    .autocorrectionDisabled()
                    .focused($focus, equals: .device)
                    .onSubmit { submit() }
            }
        }
    }

    private var tokenForm: some View {
        VStack(alignment: .leading, spacing: 14) {
            Field(label: "server url") {
                TextField("http://127.0.0.1:8848", text: urlBinding)
                    .textFieldStyle(.plain)
                    .textContentType(.URL)
                    .autocorrectionDisabled()
                    .focused($focus, equals: .url)
                    .onSubmit { focus = .token }
            }
            Field(label: "api token") {
                SecureField("SQUELCH_API_TOKEN", text: $token)
                    .textFieldStyle(.plain)
                    .focused($focus, equals: .token)
                    .onSubmit { submit() }
            }
        }
    }

    private var buttonLabel: String {
        if claiming { return "pairing…" }
        if store.connStatus == .connecting { return "testing…" }
        // The claim is done and only its probe is outstanding, so the button
        // offers the step that is actually left.
        if mode == .pair && heldToken != nil { return "connect" }
        return mode == .pair ? "pair" : "connect"
    }

    private var footnote: String {
        switch mode {
        case .pair:
            if heldToken != nil {
                return
                    "This Mac is paired already. The code is spent, so this retries the connection with the token it issued. No second code needed."
            }
            return
                "Run squelchd pair on the machine running your daemon for a code. It issues a token for this Mac only, revocable with squelchd token revoke."
        case .token:
            return "The token is stored in your macOS keychain and sent only as a bearer header."
        }
    }

    /// The mode switch, written by hand so flipping it also clears the stale
    /// error from the other path. A raw `$mode` would leave "token rejected"
    /// sitting over the pairing form.
    private var modeBinding: Binding<ConnectMode> {
        Binding(
            get: { mode },
            set: { next in
                guard next != mode else { return }
                mode = next
                pairError = nil
                store.connError = nil
                focus = next == .pair ? .code : .token
            })
    }

    /// The server-url field, hand-written because a typed edit has to drop two
    /// things a plain `$url` would leave standing: a token held from an earlier
    /// claim, which belongs to the daemon that minted it and not to whatever
    /// host the field now names, and a deep link's notice, which described a
    /// URL that is no longer what is in the field.
    private var urlBinding: Binding<String> {
        Binding(
            get: { url },
            set: { next in
                guard next != url else { return }
                url = next
                heldToken = nil
                linkHost = nil
                linkArmed = false
            })
    }

    /// The pairing-code field. A different code means a different claim, so the
    /// token the last one bought is no longer the thing a press should retry.
    /// Deliberately does NOT clear `linkHost`: typing the code is exactly the
    /// moment the warning about where that code is headed has to still be up.
    private var codeBinding: Binding<String> {
        Binding(
            get: { code },
            set: { next in
                guard next != code else { return }
                code = next
                heldToken = nil
                linkArmed = false
            })
    }

    // MARK: - actions

    private func submit() {
        guard canSubmit else { return }
        switch mode {
        case .pair: Task { await claim() }
        case .token: Task { await store.connect(serverURL: url.trimmed, apiToken: token.trimmed) }
        }
    }

    /// Claim the code, then hand the issued token to the SAME path a pasted one
    /// takes: `connect` proves it against /client/stats and stores it. Pairing
    /// adds a step in front of that flow, it does not replace it.
    ///
    /// A press AFTER the probe failed does not claim again. The token the first
    /// claim minted is held and re-probed, because a second claim would spend
    /// another of the code's five attempts and leave the first token orphaned
    /// server-side: minted, held by nobody, and only removable by hand.
    private func claim() async {
        let base = url.trimmed
        linkArmed = false
        pairError = nil
        store.connError = nil

        // Already paid for. Retry the connection, not the claim.
        if let held = heldToken {
            if await store.connect(serverURL: base, apiToken: held) { heldToken = nil }
            return
        }

        let typedCode = code
        let name = Pairing.clampDeviceName(deviceName)
        claiming = true
        do {
            let issued = try await Pairing.claim(baseURL: base, code: typedCode, deviceName: name)
            claiming = false
            // Spent the moment the daemon answers: a code is one-shot, and
            // leaving it on screen invites a second press that can only fail.
            // Assigned to the state directly rather than through `codeBinding`,
            // which is for USER edits and would drop the token we just held.
            code = ""
            heldToken = issued.token
            if await store.connect(serverURL: base, apiToken: issued.token) { heldToken = nil }
        } catch {
            claiming = false
            pairError = Pairing.message(for: error)
        }
    }

    /// Fill the form from a deep link. It NEVER claims, whatever host it names:
    /// a `passband://` URL is openable by any web page, every claim spends one
    /// of the live code's five attempts, and a 200 from whatever answers the
    /// named port is a token this app would then store. So a link gets the user
    /// a filled form with the button ringed, and no further.
    private func applyPairLink(_ link: PairLink?) {
        guard let link else { return }
        store.pairLink = nil
        guard !busy else { return }
        mode = .pair
        url = link.serverURL
        code = Pairing.formatted(link.code)
        heldToken = nil
        pairError = nil
        store.connError = nil
        // Loopback is the URL `squelchd pair` prints for its own machine, so it
        // fills quietly. Any other host was chosen by whoever wrote the link,
        // and gets said out loud before a code is typed at it.
        linkHost = link.isLoopback ? nil : link.displayHost
        linkArmed = true
        focus = .submit
    }
}

/// Which way of getting a daemon the guide is describing.
private enum GuideTab: Hashable { case hosted, selfHost }

/// What a fresh install needs to hear before the connect form makes any sense:
/// the app is a window onto a daemon, and here are the two ways to have one.
/// Instructions only — the one credential-shaped act (typing the code) still
/// happens in the form, which is where its guarantees live.
///
/// Hosted leads because it is the path that needs no terminal; the tab order is
/// the pitch. Self-host keeps the full three commands.
private struct GettingStartedPane: View {
    @State private var tab: GuideTab = .hosted

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("new here?")
                .font(Typo.serif(28, weight: .medium))
                .foregroundStyle(Palette.ink)

            GlassSegmented(
                options: [(GuideTab.hosted, "hosted"), (.selfHost, "self-host")],
                selection: $tab)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.top, 14)

            switch tab {
            case .hosted: hosted
            case .selfHost: selfHost
            }
        }
        .padding(30)
        .frame(width: 380)
        .passbandGlass(.pane, cornerRadius: 24, tint: Palette.glassTint)
        .shadow(color: .black.opacity(0.3), radius: 50, y: 24)
    }

    private var hosted: some View {
        VStack(alignment: .leading, spacing: 18) {
            Text(
                "We run the daemon for you, on your own sealed account. No server, no terminal."
            )
            .font(.system(size: 12))
            .foregroundStyle(Palette.inkFaint)
            .fixedSize(horizontal: false, vertical: true)

            GuideOption(
                title: "have a pairing code?",
                detail: "Type it into the form here and you are in.")
            GuideOption(
                title: "have an invite code?",
                detail: "Account setup takes a minute and ends with a pairing code.",
                linkLabel: "set up your account",
                linkURL: "https://signup.passband.app")
            GuideOption(
                title: "neither yet?",
                detail: "Hosted is invite-only while we grow.",
                linkLabel: "join the waitlist",
                linkURL: "https://passband.app")
        }
        .padding(.top, 16)
    }

    private var selfHost: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text(
                "The engine is squelchd, a small daemon that reads your Gmail read-only and runs on any box you own: this Mac, a NAS, a server."
            )
            .font(.system(size: 12))
            .foregroundStyle(Palette.inkFaint)
            .fixedSize(horizontal: false, vertical: true)

            GuideStep(
                number: 1, title: "run the daemon",
                detail: "One public Docker image, amd64 and arm64.",
                command: "docker pull ghcr.io/braelyn-ai/squelchd")
            GuideStep(
                number: 2, title: "authorize gmail",
                detail: "A one-time Google consent, run where the daemon lives.",
                command: "squelchd auth")
            GuideStep(
                number: 3, title: "pair this mac",
                detail: "Prints a code. Type it into the form here.",
                command: "squelchd pair")

            HStack(spacing: 14) {
                Button("full setup guide") { Opener.open("https://passband.app/self-host") }
                Button("github") { Opener.open("https://github.com/braelyn-ai/squelch") }
            }
            .buttonStyle(.textAction)
            .padding(.top, 4)
        }
        .padding(.top, 16)
    }
}

/// One either/or row on the hosted tab: a question, the one-line answer, and
/// where to go if the answer is elsewhere.
private struct GuideOption: View {
    let title: String
    let detail: String
    var linkLabel: String?
    var linkURL: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title)
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(Palette.ink)
            Text(detail)
                .font(.system(size: 11))
                .foregroundStyle(Palette.inkFaint)
                .fixedSize(horizontal: false, vertical: true)
            if let linkLabel, let linkURL {
                Button(linkLabel) { Opener.open(linkURL) }
                    .buttonStyle(.textAction)
                    .padding(.top, 2)
            }
        }
    }
}

/// One numbered step: what it is, why, and the command to copy.
private struct GuideStep: View {
    let number: Int
    let title: String
    let detail: String
    let command: String
    @State private var copied = false

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Text("\(number)")
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(Palette.accentInk)
                .frame(width: 22, height: 22)
                .background(Circle().fill(Palette.accent))
            VStack(alignment: .leading, spacing: 4) {
                Text(title)
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(Palette.ink)
                Text(detail)
                    .font(.system(size: 11))
                    .foregroundStyle(Palette.inkFaint)
                    .fixedSize(horizontal: false, vertical: true)
                commandRow
            }
        }
    }

    private var commandRow: some View {
        HStack(spacing: 8) {
            Text(command)
                .font(Typo.mono(11))
                .foregroundStyle(Palette.inkDim)
                .lineLimit(1)
                .truncationMode(.middle)
            Spacer(minLength: 4)
            Button {
                Clip.copy(command, flashing: $copied)
            } label: {
                Image(systemName: copied ? "checkmark" : "doc.on.doc")
                    .font(.system(size: 10, weight: .semibold))
                    .foregroundStyle(copied ? Palette.positive : Palette.inkFaint)
            }
            .buttonStyle(.plain)
            .help("copy command")
        }
        .padding(.horizontal, 9)
        .padding(.vertical, 6)
        .background(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .fill(Palette.canvas.opacity(0.65))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .strokeBorder(Palette.hairline, lineWidth: 0.75)
        )
        .padding(.top, 3)
    }
}

/// A labelled input well. Near-opaque on purpose: a fully translucent field
/// over a busy wallpaper leaves typed text unreadable.
struct Field<Content: View>: View {
    let label: String
    @ViewBuilder var content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            FieldLabel(label)
            content.fieldWell()
        }
    }
}

/// The caption above a well. Split out so a field needing its own row under the
/// label still labels itself identically.
struct FieldLabel: View {
    let text: String
    init(_ text: String) { self.text = text }

    var body: some View {
        Text(text)
            .font(Typo.micro)
            .foregroundStyle(Palette.inkFaint)
            .textCase(.lowercase)
    }
}

extension View {
    func fieldWell() -> some View {
        self
            .font(.system(size: 13))
            .foregroundStyle(Palette.ink)
            .padding(.horizontal, 10)
            .padding(.vertical, 8)
            .background(
                RoundedRectangle(cornerRadius: 9, style: .continuous)
                    .fill(Palette.canvas.opacity(0.65))
            )
            .overlay(
                RoundedRectangle(cornerRadius: 9, style: .continuous)
                    .strokeBorder(Palette.hairlineStrong, lineWidth: 0.75)
            )
    }
}

extension String {
    var trimmed: String { trimmingCharacters(in: .whitespacesAndNewlines) }
}
