// The connect screen. Two ways in, one destination: a bearer token in the OS
// keychain, proved against /client/stats before it is stored.
//
// TWO PLACES SHOW IT, and `purpose` is the whole difference. As the GATE it is
// the window: this install has no identity, so it also teaches what a daemon is
// and, on success, becomes the identity. As the ADD ACCOUNT sheet it sits over
// a working app and adds a SECOND daemon beside the one already live — same
// form, same proof, but it must not touch the connection state the shell behind
// it is standing on (see AppStore.addAccount).
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

/// The hosted control plane, and the two doors it opens.
///
/// SIGN IN is for somebody whose mailbox already exists: Google names them, the
/// control plane finds the daemon that mailbox owns, and the browser comes back
/// with a `passband://pair` link. No invite code anywhere in it, because an
/// invite provisions a tenant and this person's tenant is already running.
///
/// SIGN UP is the invite-code form, and it is the only place a code is asked
/// for. The two are one screen apart here for the same reason they are two
/// routes over there: they answer different questions about the same person.
private enum Hosted {
    static let signIn = "https://signup.passband.app/app/auth"
    static let signUp = "https://signup.passband.app"
}

/// Which way in the screen is showing.
private enum ConnectMode: Hashable { case pair, token }

/// The first decision a fresh install makes. It changes the explanation and
/// defaults around the credential form, never the credential protocol itself:
/// hosted and self-hosted daemons expose the same human door.
private enum HostingChoice: Hashable { case hosted, selfHosted }

/// Progressive disclosure for the first account. Add Account deliberately
/// skips this — someone adding a second daemon already knows what Passband is.
private enum ConnectGateStep: Hashable {
    case welcome
    case route(HostingChoice)
    case selfHostGuide
    /// nil means a complete pair link brought us here. Its URL is authoritative,
    /// but it does not reliably reveal whether the daemon is hosted.
    case credentials(HostingChoice?)
}

/// Why this screen is up. See the file header — it decides what a success
/// MEANS, and nothing else about the form.
enum ConnectPurpose: Hashable {
    /// The app has no identity; connecting becomes it.
    case gate
    /// The app is connected; connecting adds another account beside it.
    case addAccount
}

/// Field focus, so Enter walks the form instead of dead-ending. `submit` is the
/// button itself: a deep link fills the form and lands here, because pairing is
/// never something a link does on its own.
private enum ConnectField: Hashable { case url, code, device, token, name, submit }

struct ConnectView: View {
    @Environment(AppStore.self) private var store
    @Environment(\.dismiss) private var dismiss

    /// Defaults to the gate, so the two shells that mount it as the whole
    /// window say nothing about it.
    var purpose: ConnectPurpose = .gate

    @State private var mode: ConnectMode = .pair
    @State private var gateStep: ConnectGateStep = .welcome
    @State private var url = "http://127.0.0.1:8848"
    @State private var code = ""
    @State private var deviceName = Pairing.defaultDeviceName()
    @State private var token = ""
    /// The human's name for this account. Optional everywhere: an empty label
    /// leaves the switcher showing the daemon's host:port, which is a real
    /// answer rather than a placeholder.
    @State private var accountLabel = ""
    /// An add-account attempt is in flight. The gate reads `store.connStatus`
    /// for this, but adding deliberately never moves it — the shell behind the
    /// sheet is standing on it.
    @State private var adding = false
    /// The failure from an add. Kept out of `store.connError`, which belongs to
    /// the gate and would still be sitting there next time one opened.
    @State private var addError: String?
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
    @State private var pairingHelp = false
    @FocusState private var focus: ConnectField?

    private var busy: Bool { claiming || adding || store.connStatus == .connecting }

    /// One error line, whichever half produced it. A pairing failure wins: it is
    /// the more recent thing the user did.
    private var errorText: String? { pairError ?? addError ?? store.connError }

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
        Group {
            switch purpose {
            case .gate: gateBody
            // A sheet is sized by its content, so the card's own measure is the
            // sheet's — no filling the screen, and no getting-started pane: an
            // install adding a SECOND daemon plainly knows what the first one
            // was.
            case .addAccount: connectCard.padding(24)
            }
        }
        // A link can arrive before this view mounts (the app was launched by
        // one, or the sheet is being opened BY one) or while it is up, so both
        // entry points are covered.
        .task { applyPairLink(store.pairLink) }
        .onChange(of: store.pairLink) { _, link in applyPairLink(link) }
    }

    private var gateBody: some View {
        ZStack {
            switch gateStep {
            case .welcome:
                WelcomeGate { choice in
                    url = choice == .selfHosted ? "http://127.0.0.1:8848" : ""
                    gateStep = .route(choice)
                }
            case .route(let choice):
                RouteGate(
                    choice: choice,
                    back: { gateStep = .welcome },
                    continueToForm: {
                        mode = .pair
                        gateStep = .credentials(choice)
                    },
                    showSelfHostGuide: { gateStep = .selfHostGuide })
            case .selfHostGuide:
                SelfHostGuide(
                    back: { gateStep = .route(.selfHosted) },
                    continueToForm: {
                        mode = .pair
                        gateStep = .credentials(.selfHosted)
                    })
            case .credentials:
                connectCard
            }
        }
        // The phone's screen margin. On the Mac the card is a fixed measure
        // floating in a large window and needs none.
        #if !os(macOS)
            .padding(.horizontal, 16)
        #endif
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .overlay(alignment: .bottomTrailing) {
            SetupThemeToggle()
                .padding(20)
        }
    }

    private var connectCard: some View {
            VStack(spacing: 0) {
                header
                linkNotice
                credentialContent

                if let errorText {
                    Label(errorText, systemImage: "exclamationmark.triangle.fill")
                        .font(.system(size: 12))
                        .foregroundStyle(Palette.danger)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.top, 12)
                }

                bottomActions.padding(.top, 20)
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
            .passbandGlass(
                purpose == .gate ? .chrome : .pane,
                cornerRadius: 24,
                tint: purpose == .gate ? Palette.glassTint.opacity(0.35) : Palette.glassTintStrong)
            .shadow(color: .black.opacity(0.3), radius: 50, y: 24)
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(headerTitle)
                .font(Typo.serif(purpose == .gate ? 34 : 30, weight: .medium))
                .foregroundStyle(Palette.ink)
            Text(
                purpose == .gate
                    ? headerSubtitle
                    : "one daemon per mailbox. this one joins the accounts you already have."
            )
            .font(.system(size: 13))
            .foregroundStyle(Palette.inkFaint)
            .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.bottom, 22)
    }

    private var hostingChoice: HostingChoice? {
        guard purpose == .gate, case .credentials(let choice) = gateStep else { return nil }
        return choice
    }

    private var headerTitle: String {
        guard purpose == .gate else { return "add account" }
        switch hostingChoice {
        case .hosted: return "connect hosted"
        case .selfHosted: return "connect self-hosted"
        case nil: return "pair this device"
        }
    }

    private var headerSubtitle: String {
        switch hostingChoice {
        case .hosted:
            return "Sign in with the Google account your mailbox belongs to, and we will connect this device."
        case .selfHosted:
            return "Run squelchd pair on your server, then enter what it prints."
        case nil:
            return "Check the server below, then confirm the pairing link."
        }
    }

    @ViewBuilder private var credentialContent: some View {
        if hostingChoice == .hosted {
            // THE WHOLE HOSTED SIGN IN. It leaves the app because Google's
            // consent has to run in a real browser the user can inspect, and it
            // comes back on its own: the page at the end of it carries a
            // `passband://pair` link, which lands in `applyPairLink` below and
            // fills the form under this button. Nothing is asked for here in the
            // meantime, which is the point — an existing account has no invite
            // code to find and no pairing code to go and mint.
            Button("sign in with google") {
                Opener.open(Hosted.signIn)
            }
            .buttonStyle(.borderedProminent)
            .tint(Palette.accent)
            .controlSize(.large)
            .frame(maxWidth: .infinity)

            Text("Your browser opens, you pick your Google account, and the page it lands on brings you back here.")
                .font(.system(size: 11))
                .foregroundStyle(Palette.inkFaintest)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.top, 9)

            HStack(spacing: 10) {
                Rectangle().fill(Palette.hairline).frame(height: 0.75)
                Text("or")
                    .font(.system(size: 11))
                    .foregroundStyle(Palette.inkFaintest)
                Rectangle().fill(Palette.hairline).frame(height: 0.75)
            }
            .padding(.vertical, 15)

            hostedManualForm
        } else {
            if purpose == .addAccount || hostingChoice == .selfHosted {
                GlassSegmented(
                    options: [(ConnectMode.pair, "pair with code"), (.token, "api token")],
                    selection: modeBinding)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.bottom, 16)
            }
            switch mode {
            case .pair: pairForm
            case .token: tokenForm
            }
            nameField.padding(.top, 14)
        }
    }

    private var bottomActions: some View {
        HStack(spacing: 12) {
            Button(purpose == .addAccount ? "cancel" : "back") { goBack() }
                .disabled(busy)
            Spacer()
            Button(buttonLabel) { submit() }
                .buttonStyle(.borderedProminent)
                .tint(Palette.accent)
                .disabled(!canSubmit)
                .focused($focus, equals: .submit)
                .overlay {
                    if linkArmed {
                        RoundedRectangle(cornerRadius: 7, style: .continuous)
                            .strokeBorder(Palette.accent, lineWidth: 2)
                            .padding(-4)
                            .allowsHitTesting(false)
                    }
                }
        }
    }

    private func goBack() {
        guard purpose == .gate else { dismiss(); return }
        pairError = nil
        addError = nil
        store.connError = nil
        if let choice = hostingChoice { gateStep = .route(choice) }
        else { gateStep = .welcome }
    }

    /// The account's name, optional in both purposes. It matters most in the
    /// sheet — a second row in the switcher wants a word a human picked — but
    /// the first account is allowed one too, since it is about to have company.
    private var nameField: some View {
        Field(label: "account name") {
            TextField("optional, defaults to the server host", text: $accountLabel)
                .textFieldStyle(.plain)
                .autocorrectionDisabled()
                .focused($focus, equals: .name)
                .onSubmit { submit() }
        }
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
                TextField(serverPlaceholder, text: urlBinding)
                    .textFieldStyle(.plain)
                    .textContentType(.URL)
                    .autocorrectionDisabled()
                    .focused($focus, equals: .url)
                    .onSubmit { focus = .code }
            }
            VStack(alignment: .leading, spacing: 5) {
                HStack(spacing: 5) {
                    FieldLabel("pairing code")
                    if hostingChoice == .selfHosted {
                        Button { pairingHelp = true } label: {
                            Image(systemName: "questionmark.circle")
                                .font(.system(size: 11))
                        }
                        .buttonStyle(.plain)
                        .foregroundStyle(Palette.inkFaint)
                        .popover(isPresented: $pairingHelp) {
                            VStack(alignment: .leading, spacing: 10) {
                                Text("Run this on the machine hosting squelchd:")
                                    .font(.system(size: 12))
                                Text("squelchd pair")
                                    .font(Typo.mono(13))
                                    .padding(.horizontal, 10)
                                    .padding(.vertical, 7)
                                    .background(RoundedRectangle(cornerRadius: 8).fill(Palette.canvas))
                            }
                            .padding(16)
                        }
                    }
                }
                TextField("XXXX-XXXX", text: codeBinding)
                    .textFieldStyle(.plain)
                    // Monospaced so a code read off a terminal lines up with
                    // what the terminal showed, character for character.
                    .font(Typo.mono(13))
                    .autocorrectionDisabled()
                    .focused($focus, equals: .code)
                    .onSubmit { submit() }
                    .fieldWell()
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

    /// Hosted's manual escape hatch, for the cases the sign in above cannot
    /// cover: a browser that will not open app links, a sign in finished on a
    /// phone for a Mac, or a code from the console's own Add Device button.
    /// Device and account names keep their safe defaults; hiding those optional
    /// fields makes this visibly secondary to the browser handoff instead of
    /// presenting another full setup ceremony.
    private var hostedManualForm: some View {
        VStack(alignment: .leading, spacing: 10) {
            Field(label: "server url") {
                TextField("https://username.passband.email", text: urlBinding)
                    .textFieldStyle(.plain)
                    .textContentType(.URL)
                    .autocorrectionDisabled()
                    .focused($focus, equals: .url)
                    .onSubmit { focus = .code }
            }
            Field(label: "pairing code") {
                TextField("XXXX-XXXX", text: codeBinding)
                    .textFieldStyle(.plain)
                    .font(Typo.mono(13))
                    .autocorrectionDisabled()
                    .focused($focus, equals: .code)
                    .onSubmit { submit() }
            }
        }
    }

    private var serverPlaceholder: String {
        hostingChoice == .hosted
            ? "https://username.passband.email"
            : "http://127.0.0.1:8848"
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
        if adding || store.connStatus == .connecting { return "testing…" }
        // The claim is done and only its probe is outstanding, so the button
        // offers the step that is actually left.
        let done = purpose == .gate ? "connect" : "add account"
        if mode == .pair && heldToken != nil { return done }
        return mode == .pair ? "pair" : done
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
                addError = nil
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
        case .token: Task { await finish(serverURL: url.trimmed, apiToken: token.trimmed) }
        }
    }

    /// THE destination both forms share, and the one place `purpose` changes
    /// what happens: the gate's credentials become this install's identity, the
    /// sheet's become one more account beside it. Returns whether the
    /// credentials were accepted, which is what tells `claim` its held token is
    /// spent.
    @discardableResult
    private func finish(serverURL: String, apiToken: String) async -> Bool {
        addError = nil
        switch purpose {
        case .gate:
            return await store.connect(
                serverURL: serverURL, apiToken: apiToken, label: accountLabel.trimmed)
        case .addAccount:
            adding = true
            let outcome = await store.addAccount(
                serverURL: serverURL, apiToken: apiToken, label: accountLabel.trimmed)
            adding = false
            addError = outcome.error
            // The sheet's whole job is done, and the app behind it has already
            // switched to the account that was just added.
            if outcome.ok { dismiss() }
            return outcome.ok
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
        addError = nil
        store.connError = nil

        // Already paid for. Retry the connection, not the claim.
        if let held = heldToken {
            if await finish(serverURL: base, apiToken: held) { heldToken = nil }
            return
        }

        // A duplicate daemon must be refused BEFORE the claim, not after:
        // claiming mints a device token on the daemon and spends one of the
        // code's five attempts, and `addAccount`'s own check would only see
        // the duplicate once both are already gone.
        if purpose == .addAccount, store.isKnownDaemon(base) {
            addError = "that daemon is already one of your accounts"
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
            if await finish(serverURL: base, apiToken: issued.token) { heldToken = nil }
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
        if purpose == .gate { gateStep = .credentials(nil) }
        guard !busy else { return }
        mode = .pair
        url = link.serverURL
        code = Pairing.formatted(link.code)
        heldToken = nil
        pairError = nil
        addError = nil
        store.connError = nil
        // Loopback is the URL `squelchd pair` prints for its own machine, so it
        // fills quietly. Any other host was chosen by whoever wrote the link,
        // and gets said out loud before a code is typed at it.
        linkHost = link.isLoopback ? nil : link.displayHost
        linkArmed = true
        // At the gate the form is the whole screen and Return is the natural
        // next act. The Add Account sheet is raised OVER whatever the human
        // was doing, and focusing submit there would let a Return already in
        // flight claim against the link's chosen host — pairing from a link
        // while connected takes an explicit click.
        focus = purpose == .gate ? .submit : nil
    }
}

/// The calm first screen: one product sentence and the only decision needed to
/// tailor what follows. Credentials stay off-screen until a route is chosen.
private struct WelcomeGate: View {
    let choose: (HostingChoice) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("welcome to passband")
                .font(Typo.serif(40, weight: .medium))
                .foregroundStyle(Palette.ink)
            Text("A private, focused way to read and act on your inbox.")
                .font(.system(size: 14))
                .foregroundStyle(Palette.inkFaint)
                .padding(.top, 6)

            Text("Where should your mail service run?")
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(Palette.inkDim)
                .padding(.top, 30)

            VStack(spacing: 12) {
                ChoiceCard(
                    symbol: "cloud",
                    title: "use hosted passband",
                    detail: "We run it for you. No server or terminal required.",
                    action: { choose(.hosted) })
                ChoiceCard(
                    symbol: "server.rack",
                    title: "self-host",
                    detail: "Run squelchd on this Mac, a NAS, or your own server.",
                    action: { choose(.selfHosted) })
            }
            .padding(.top, 12)

            Text("Both paths use the same Passband app and support per-device pairing.")
                .font(.system(size: 11))
                .foregroundStyle(Palette.inkFaintest)
                .padding(.top, 18)
        }
        .padding(34)
        #if os(macOS)
            .frame(width: 600)
        #else
            .frame(maxWidth: 600)
        #endif
        .passbandGlass(.chrome, cornerRadius: 24, tint: Palette.glassTint.opacity(0.35))
        .shadow(color: .black.opacity(0.3), radius: 50, y: 24)
    }
}

/// The route-specific fork before credentials. These are decisions, not tabs:
/// one continues in-app and the other opens the setup surface that must finish
/// before a credential can exist.
private struct RouteGate: View {
    let choice: HostingChoice
    let back: () -> Void
    let continueToForm: () -> Void
    let showSelfHostGuide: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(choice == .hosted ? "hosted passband" : "self-hosted")
                .font(Typo.serif(34, weight: .medium))
                .foregroundStyle(Palette.ink)
            Text(choice == .hosted
                ? "Connect an existing account or create a new one."
                : "Connect a running squelch server or set one up first.")
                .font(.system(size: 13))
                .foregroundStyle(Palette.inkFaint)
                .padding(.top, 5)

            HStack(alignment: .top, spacing: 12) {
                if choice == .hosted {
                    RouteOption(
                        symbol: "person.crop.circle",
                        title: "Login",
                        action: continueToForm)
                    RouteOption(
                        symbol: "person.badge.plus",
                        title: "Sign up",
                        action: { Opener.open(Hosted.signUp) })
                } else {
                    RouteOption(
                        symbol: "checkmark.circle",
                        title: "i have a squelch server running",
                        action: continueToForm)
                    RouteOption(
                        symbol: "server.rack",
                        title: "i need to set up my server",
                        action: showSelfHostGuide)
                }
            }
            .padding(.top, 24)

            Button("back", action: back)
                .padding(.top, 22)
        }
        .padding(34)
        #if os(macOS)
            .frame(width: 680)
        #else
            .frame(maxWidth: 680)
        #endif
        .passbandGlass(.chrome, cornerRadius: 24, tint: Palette.glassTint.opacity(0.35))
        .shadow(color: .black.opacity(0.3), radius: 50, y: 24)
    }
}

private struct RouteOption: View {
    let symbol: String
    let title: String
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 11) {
                Image(systemName: symbol)
                    .font(.system(size: 25, weight: .medium))
                    .foregroundStyle(Palette.accent)
                    .frame(width: 30)
                Text(title)
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(Palette.ink)
                    .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: 0)
            }
            .frame(maxWidth: .infinity, minHeight: 34, alignment: .leading)
            .padding(.horizontal, 15)
            .padding(.vertical, 10)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .background(RoundedRectangle(cornerRadius: 16).fill(Palette.canvas.opacity(0.62)))
        .overlay(RoundedRectangle(cornerRadius: 16).strokeBorder(Palette.hairline, lineWidth: 0.75))
    }
}

/// Self-hosting stays inside Passband: the shortest complete path from no
/// daemon to a pairing code, with copyable commands and an explicit handoff to
/// the form once the server is running.
private struct SelfHostGuide: View {
    let back: () -> Void
    let continueToForm: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("set up your server")
                .font(Typo.serif(34, weight: .medium))
                .foregroundStyle(Palette.ink)
            Text("Run squelchd on this Mac, a NAS, or a server you control.")
                .font(.system(size: 13))
                .foregroundStyle(Palette.inkFaint)
                .padding(.top, 5)

            VStack(spacing: 12) {
                SetupStep(
                    number: 1,
                    title: "install squelchd",
                    detail: "Pull the public image for amd64 or arm64.",
                    command: "docker pull ghcr.io/braelyn-ai/squelchd")
                SetupStep(
                    number: 2,
                    title: "authorize gmail",
                    detail: "Complete Google's one-time consent on the server.",
                    command: "squelchd auth")
                SetupStep(
                    number: 3,
                    title: "pair this device",
                    detail: "Mint the short code the next screen accepts.",
                    command: "squelchd pair")
            }
            .padding(.top, 22)

            Button("view detailed instructions on GitHub") {
                Opener.open(
                    "https://github.com/braelyn-ai/squelch/blob/main/docs/GETTING-STARTED.md")
            }
            .buttonStyle(.textAction)
            .padding(.top, 16)

            HStack {
                Button("back", action: back)
                Spacer()
                Button("my server is running", action: continueToForm)
                    .buttonStyle(.borderedProminent)
                    .tint(Palette.accent)
            }
            .padding(.top, 22)
        }
        .padding(34)
        #if os(macOS)
            .frame(width: 620)
        #else
            .frame(maxWidth: 620)
        #endif
        .passbandGlass(.chrome, cornerRadius: 24, tint: Palette.glassTint.opacity(0.35))
        .shadow(color: .black.opacity(0.3), radius: 50, y: 24)
    }
}

private struct SetupStep: View {
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
                .frame(width: 23, height: 23)
                .background(Circle().fill(Palette.accent))
            VStack(alignment: .leading, spacing: 4) {
                Text(title)
                    .font(.system(size: 13, weight: .semibold))
                Text(detail)
                    .font(.system(size: 11))
                    .foregroundStyle(Palette.inkFaint)
                HStack(spacing: 8) {
                    Text(command)
                        .font(Typo.mono(11))
                        .lineLimit(1)
                        .truncationMode(.middle)
                    Spacer(minLength: 4)
                    Button {
                        Clip.copy(command, flashing: $copied)
                    } label: {
                        Image(systemName: copied ? "checkmark" : "doc.on.doc")
                    }
                    .buttonStyle(.plain)
                }
                .padding(.horizontal, 9)
                .padding(.vertical, 6)
                .background(RoundedRectangle(cornerRadius: 8).fill(Palette.canvas.opacity(0.65)))
                .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Palette.hairline, lineWidth: 0.75))
                .padding(.top, 2)
            }
        }
    }
}

/// Always reachable during first-run setup, before Settings exists as a
/// destination. It uses the same persisted preference and flip semantics as
/// the app-wide keyboard shortcut.
private struct SetupThemeToggle: View {
    @Environment(Prefs.self) private var prefs

    private var isDark: Bool {
        switch prefs.theme {
        case .dark: true
        case .light: false
        case .system: Platform.isDarkAppearance
        }
    }

    var body: some View {
        Button { prefs.flipTheme() } label: {
            Image(systemName: isDark ? "sun.max.fill" : "moon.fill")
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(Palette.ink)
                .frame(width: 30, height: 30)
        }
        .buttonStyle(.plain)
        .glassCapsule(tint: Palette.glassTint.opacity(0.35))
        .help(isDark ? "use light mode" : "use dark mode")
        .accessibilityLabel(isDark ? "Use light mode" : "Use dark mode")
    }
}

private struct ChoiceCard: View {
    let symbol: String
    let title: String
    let detail: String
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 15) {
                Image(systemName: symbol)
                    .font(.system(size: 20, weight: .medium))
                    .foregroundStyle(Palette.accent)
                    .frame(width: 32)
                VStack(alignment: .leading, spacing: 4) {
                    Text(title)
                        .font(.system(size: 14, weight: .semibold))
                        .foregroundStyle(Palette.ink)
                    Text(detail)
                        .font(.system(size: 12))
                        .foregroundStyle(Palette.inkFaint)
                        .fixedSize(horizontal: false, vertical: true)
                }
                Spacer(minLength: 12)
                Image(systemName: "chevron.right")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Palette.inkFaintest)
            }
            .padding(18)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .background(
            RoundedRectangle(cornerRadius: 16, style: .continuous)
                .fill(Palette.canvas.opacity(0.62))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 16, style: .continuous)
                .strokeBorder(Palette.hairline, lineWidth: 0.75)
        )
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
