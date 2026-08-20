// The glass layer. `.glassEffect` is an AppKit material that samples the REAL
// window backdrop, so it only reads over a non-opaque window (WindowBackdrop).
// Semantic surfaces tint the material itself rather than painting over it.
// Adjacent glass inside a `GlassEffectContainer` merges and separates as it
// moves; `glassEffectID(_:in:)` with a shared `@Namespace` morphs one shape
// into another instead of cross-fading.

import SwiftUI

// MARK: - surface vocabulary

/// How much presence a glass surface has. Maps onto the two real materials
/// (`.regular` / `.clear`) plus a tint.
enum GlassLevel {
    /// A primary content pane: a zone card, a modal, the reader chrome.
    case pane
    /// A surface nested INSIDE a pane (a nested well, a record row).
    case nested
    /// Chrome that must stay maximally see-through: the rail, chips.
    case chrome
}

extension View {
    /// The app's standard glass card. Pass a tier color for semantic surfaces,
    /// nil for a neutral pane.
    func passbandGlass(
        _ level: GlassLevel = .pane,
        cornerRadius: CGFloat = 16,
        tint: Color? = Palette.glassTint,
        interactive: Bool = false
    ) -> some View {
        let base: Glass = level == .chrome ? .clear : .regular
        var glass = base
        if let tint { glass = glass.tint(tint) }
        if interactive { glass = glass.interactive() }
        return self.glassEffect(
            glass, in: .rect(cornerRadius: cornerRadius, style: .continuous))
    }

    /// A glass surface participating in matched-geometry morphing. Pair with a
    /// `GlassEffectContainer` and a shared `@Namespace`.
    func passbandGlass<ID: Hashable & Sendable>(
        _ level: GlassLevel = .pane,
        cornerRadius: CGFloat = 16,
        tint: Color? = Palette.glassTint,
        interactive: Bool = false,
        id: ID,
        in namespace: Namespace.ID
    ) -> some View {
        passbandGlass(level, cornerRadius: cornerRadius, tint: tint, interactive: interactive)
            .glassEffectID(id, in: namespace)
    }

    /// A capsule of glass — chips, pills, the status strip's buttons.
    func glassCapsule(tint: Color? = nil, interactive: Bool = true) -> some View {
        var glass = Glass.clear
        if let tint { glass = glass.tint(tint) }
        if interactive { glass = glass.interactive() }
        return self.glassEffect(glass, in: .capsule)
    }

    /// Standard content inset for a glass zone card.
    func zonePadding() -> some View {
        padding(.horizontal, 16).padding(.vertical, 14)
    }

    /// Room for glass shadows in a scrolling column. A ScrollView clips at its
    /// bounds, and a column's bounds sit AT the card edges — so the material's
    /// intrinsic shadow shears off into a hard-edged dark band down both sides
    /// of the column. Disabling the scroll clip and re-masking wider lets the
    /// shadow bleed sideways and off the bottom.
    ///
    /// The top cannot be handled the same way: extended, scrolled-away cards
    /// would ride up over the chrome above the column; tight, a HARD mask edge
    /// shears the diffuse shadow into a visible line — at rest (the shadow
    /// projects upward too) and worse mid-scroll, when the shear line slides
    /// along the card. So the top is a short GRADIENT instead: card and shadow
    /// fade out together as they leave, and no hard line ever exists.
    ///
    /// The caller owes the fade its geometry on BOTH sides of the viewport
    /// edge: `topFade` of top padding INSIDE the scroll content, so resting
    /// cards sit below the fade at full opacity — and a standoff OUTSIDE the
    /// scroll view (padding on the chrome above), because the fade reaches
    /// zero exactly at the viewport top, so whatever gap separates that edge
    /// from the chrome is all the breathing room scrolling content ever gets.
    func scrollShadowRoom(_ amount: CGFloat = 40, topFade: CGFloat = 14) -> some View {
        scrollClipDisabled()
            .mask {
                VStack(spacing: 0) {
                    LinearGradient(
                        colors: [.clear, .black], startPoint: .top, endPoint: .bottom
                    )
                    .frame(height: topFade)
                    Color.black
                }
                .padding(.horizontal, -amount)
                .padding(.bottom, -amount)
            }
    }

    /// Selection + hover for a list row: a tinted fill, ONE view identity, and
    /// no material anywhere.
    ///
    /// NOT glass and NOT a branch, deliberately, because each costs the row
    /// twice over. An `if selected` branch is two view identities, so flipping
    /// `selected` tears down and rebuilds the row's whole subtree — `@State`
    /// reset, `.task` re-run, layout rebuilt. And glass on a row means a
    /// `GlassEffectContainer` around the list to morph it, which re-coordinates
    /// every descendant whenever ANY of them changes; a hovered row changes, so
    /// a mouse sweep over a list pays a glass pass per event even while nothing
    /// in the container is selected. At row size the material buys nothing a
    /// tint doesn't: it reads as a coloured rectangle anyway.
    ///
    /// One identity also means these alphas interpolate, which a branch never
    /// could.
    func selectionFill(
        _ selected: Bool,
        hovering: Bool = false,
        tint: Color = Palette.accent,
        cornerRadius: CGFloat = 9
    ) -> some View {
        background(
            RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
                .fill(
                    selected
                        ? tint.opacity(SelectionTone.selected)
                        : (hovering ? Palette.hairline.opacity(SelectionTone.hover) : .clear))
        )
        .overlay(
            RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
                .strokeBorder(
                    selected ? tint.opacity(SelectionTone.border) : .clear, lineWidth: 1)
        )
    }
}

/// Row selection alphas. These carry ALL of a selection's weight now that no
/// material sits under them, so they are the knob to turn if selection reads
/// too faint or too loud.
enum SelectionTone {
    static let selected: Double = 0.3
    static let border: Double = 0.45
    static let hover: Double = 0.55
}

// MARK: - zone card

/// A sitrep zone: heading with a glyph + count, then content, on one tinted
/// glass pane.
struct ZoneCard<Content: View>: View {
    let symbol: String
    let title: String
    var count: Int?
    var subtitle: String?
    /// A semantic tint; nil uses the standard passband wash.
    var tint: Color?
    var trailing: AnyView?
    @ViewBuilder var content: Content

    init(
        symbol: String,
        title: String,
        count: Int? = nil,
        subtitle: String? = nil,
        tint: Color? = nil,
        trailing: AnyView? = nil,
        @ViewBuilder content: () -> Content
    ) {
        self.symbol = symbol
        self.title = title
        self.count = count
        self.subtitle = subtitle
        self.tint = tint
        self.trailing = trailing
        self.content = content()
    }

    /// The zone's supplementary line, styled once and placed differently per
    /// shell (see the header below).
    @ViewBuilder private var subtitleText: some View {
        if let subtitle {
            Text(subtitle)
                .font(Typo.micro)
                .foregroundStyle(Palette.inkFaintest)
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            // THE SUBTITLE DROPS TO ITS OWN LINE ON THE PHONE. A Mac zone header
            // is one row because the pane is wide enough to hold title, count,
            // subtitle and the trailing door side by side. At phone width that
            // same row wraps the subtitle to three lines and shoves the door off
            // the edge, so the phone spends a line rather than mangling four
            // things into one.
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 8) {
                    Image(systemName: symbol)
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(tint ?? Palette.accent)
                    Text(title)
                        .font(Typo.zoneTitle)
                        .foregroundStyle(Palette.ink)
                        // Pin the casing: `textCase` is an ENVIRONMENT value, so an
                        // ancestor could silently uppercase every zone heading.
                        // Zone titles are sentence case by design.
                        .textCase(nil)
                    if let count, count > 0 {
                        Text("\(count)")
                            .font(Typo.num(11, weight: .semibold))
                            .foregroundStyle(Palette.inkFaint)
                            .padding(.horizontal, 6)
                            .padding(.vertical, 1)
                            .background(Capsule().fill(Palette.hairline))
                    }
                    #if os(macOS)
                        subtitleText
                    #endif
                    Spacer(minLength: 4)
                    if let trailing { trailing }
                }
                #if os(iOS)
                    subtitleText
                #endif
            }
            content
        }
        .zonePadding()
        .frame(maxWidth: .infinity, alignment: .leading)
        .passbandGlass(.pane, cornerRadius: 18, tint: tint?.opacity(0.12) ?? Palette.glassTint)
    }
}

// MARK: - chips

/// A small status chip; `tone` carries the tier semantics.
struct Chip: View {
    let text: String
    var tone: Color = Palette.inkFaint
    var symbol: String?
    var filled: Bool = false

    var body: some View {
        HStack(spacing: 3) {
            if let symbol {
                Image(systemName: symbol).font(.system(size: 9, weight: .semibold))
            }
            Text(text)
        }
        .font(Typo.micro)
        .foregroundStyle(tone)
        .padding(.horizontal, 7)
        .padding(.vertical, 2.5)
        .background(
            Capsule().fill(filled ? tone.opacity(0.16) : Color.clear)
        )
        .overlay(
            Capsule().strokeBorder(tone.opacity(filled ? 0.28 : 0.22), lineWidth: 0.75)
        )
        .fixedSize()
    }
}

// MARK: - overlay scaffold

/// The canonical modal scaffold: a dimming scrim that closes on click, with a
/// glass card centered (or top-anchored, for command bars) above it. The
/// contract is conditional-mount by the parent, its own "modal" KeyContext, and
/// Esc + backdrop-click to close.
struct OverlayScrim<Content: View>: View {
    var alignment: Alignment = .center
    var topInset: CGFloat = 0
    let onDismiss: () -> Void
    @ViewBuilder var content: Content

    var body: some View {
        ZStack(alignment: alignment) {
            // DIM ONLY — the defocus is done by blurring the content itself,
            // not by stacking a material here. A material is not a blur:
            // `.thinMaterial` flattens the board into an opaque slab and
            // `.ultraThinMaterial` reads as nothing, and neither keeps the app
            // visible behind the modal.
            Rectangle()
                .fill(.black.opacity(0.14))
                .ignoresSafeArea()
                .contentShape(Rectangle())
                .onTapGesture(perform: onDismiss)
            content
                .padding(.top, topInset)
        }
        .transition(.opacity.combined(with: .scale(scale: 0.985, anchor: .center)))
    }
}

/// A modal's glass card body.
struct ModalCard<Content: View>: View {
    var width: CGFloat = 460
    var tint: Color? = Palette.glassTint
    @ViewBuilder var content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 12) { content }
            .padding(18)
            // A MEASURE ON THE MAC, A CEILING ON THE PHONE. Every card in this app
            // is sized to read well at a desk, and the narrowest of them is still
            // wider than a phone — so on iOS the number becomes the most it may
            // take rather than what it takes, and the screen decides the rest.
            // Same treatment ConnectView's 440 got when the phone target landed.
            #if os(macOS)
                .frame(width: width)
            #else
                .frame(maxWidth: width)
                .padding(.horizontal, 12)
            #endif
            .passbandGlass(.pane, cornerRadius: 20, tint: tint)
            .shadow(color: .black.opacity(0.28), radius: 40, y: 18)
    }
}

// MARK: - native window backdrop

// Everything above is the glass vocabulary itself and stays shared. This piece
// is about the WINDOW — a desktop object — so it is fenced; an iOS shell brings
// its own ground for the same glass to sit on.
#if os(macOS)
    /// The AppKit backdrop the whole glass language sits ON, plus the app's own
    /// tint over it. A non-opaque window over a `.underWindowBackground` visual-
    /// effect view is what lets `.glassEffect` sample real desktop content.
    ///
    /// The scrim alphas are tuned against the hard case — a dark, busy wallpaper
    /// behind a light-theme window — because much below these values the backdrop
    /// bleeds through and light mode reads as a muddy dark one; much above and the
    /// wallpaper stops reading through at all.
    struct WindowBackdrop: View {
        /// THE APP'S TRANSPARENCY KNOBS — lower is more wallpaper. There is a floor:
        /// far below these the backdrop bleeds through until light mode reads as a
        /// muddy dark one over a dark wallpaper, which is the case they are tuned
        /// against. The canvas carries the theme, so it stays denser than the wash.
        static let canvasTop: Double = 0.54
        static let canvasBottom: Double = 0.6
        static let brandWash: Double = 0.44

        var body: some View {
            ZStack {
                VisualEffectBackdrop()
                // The canvas is blue, not neutral: light mode a pale sky deepening
                // toward the bottom, dark mode a blue-black rather than a true
                // black, so the accent still has somewhere to sit.
                LinearGradient(
                    colors: [
                        Color(light: 0xE6EFFC, dark: 0x0C1420).opacity(Self.canvasTop),
                        Color(light: 0xD5E5F8, dark: 0x080E19).opacity(Self.canvasBottom),
                    ],
                    startPoint: .top, endPoint: .bottom)
                // The brand wash on top, so empty window area is passband's too.
                Palette.glassTint.opacity(Self.brandWash)
            }
        }
    }
#endif

// MARK: - text actions

/// An action that is nothing but its own label — no pill, no border, no fill,
/// for chrome sitting directly above content the reader is reading. With no
/// shape of its own the color change is the whole affordance: faint at rest so
/// it recedes, accent on hover. Key hints (`Kbd`) keep their own chip.
struct TextActionStyle: ButtonStyle {
    @State private var hovering = false

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .foregroundStyle(hovering ? Palette.accent : Palette.inkFaint)
            .opacity(configuration.isPressed ? 0.55 : 1)
            // Without this the glyphs are the only hit target, so hover
            // flickers between letters.
            .contentShape(Rectangle())
            .onHover { hovering = $0 }
    }
}

extension ButtonStyle where Self == TextActionStyle {
    static var textAction: TextActionStyle { TextActionStyle() }
}

// The two AppKit representables the shell is built from. Both reach for the
// hosting NSWindow, which has no counterpart to shim: an iOS app has no window
// to make non-opaque and no titlebar to hide.
#if os(macOS)
    private struct VisualEffectBackdrop: NSViewRepresentable {
        func makeNSView(context: Context) -> NSVisualEffectView {
            let view = NSVisualEffectView()
            view.material = .underWindowBackground
            view.blendingMode = .behindWindow
            // Stay frosted when the window loses focus: the sitrep is glanceable,
            // so it has to stay legible in the background.
            view.state = .active
            return view
        }

        func updateNSView(_ nsView: NSVisualEffectView, context: Context) {}
    }

    /// Window-mode state the LAYOUT reads — today just fullscreen, which is what
    /// decides whether the title strip above the rail exists at all. Written by
    /// WindowConfigurator's notification observers, read from view bodies
    /// (Observation tracks the access); one instance because there is one shell
    /// window.
    @Observable @MainActor
    final class WindowState {
        static let shared = WindowState()
        /// True from willEnterFullScreen to willExitFullScreen: no traffic
        /// lights are on screen, so nothing reserves the strip they live in.
        var isFullscreen = false
    }

    /// Reaches the hosting NSWindow once, to make it non-opaque (without which the
    /// glass has nothing to sample) and to hide the title bar.
    struct WindowConfigurator: NSViewRepresentable {
        func makeNSView(context: Context) -> NSView {
            let view = NSView()
            DispatchQueue.main.async {
                guard let window = view.window else { return }
                window.isOpaque = false
                window.backgroundColor = .clear
                window.titlebarAppearsTransparent = true
                window.titleVisibility = .hidden
                window.styleMask.insert(.fullSizeContentView)
                window.isMovableByWindowBackground = true
                // Restoration can hand the window back already fullscreen,
                // before any transition our observers could hear.
                WindowState.shared.isFullscreen = window.styleMask.contains(.fullScreen)
                Self.hideTitlebarDecoration(in: window)
            }
            return view
        }

        /// Hide the glass chip macOS 26 paints behind the traffic lights
        /// (`_NSTitlebarDecorationView`). It samples the backdrop and re-tints it,
        /// so the title strip reads as a different colour than the page it sits on.
        /// `titlebarAppearsTransparent` hides the titlebar *background* view but
        /// not this one, and no public API reaches it, hence the class-name walk.
        ///
        /// AppKit builds the decoration lazily — it does not exist yet when the
        /// window is configured — and can rebuild it on later transitions, so one
        /// walk is not enough: a few post-launch retries catch the initial build,
        /// and key/main/occlusion observers catch rebuilds for the window's life.
        static func hideTitlebarDecoration(in window: NSWindow) {
            hideDecoration(in: window)
            alignTrafficLights(in: window)
            for delay in [0.25, 0.75, 2.0] {
                DispatchQueue.main.asyncAfter(deadline: .now() + delay) {
                    MainActor.assumeIsolated { hideAllDecorations() }
                }
            }
            for name in [
                NSWindow.didBecomeKeyNotification, NSWindow.didResignKeyNotification,
                NSWindow.didBecomeMainNotification, NSWindow.didResignMainNotification,
                NSWindow.didChangeOcclusionStateNotification,
            ] {
                NotificationCenter.default.addObserver(
                    forName: name, object: window, queue: .main
                ) { _ in
                    MainActor.assumeIsolated { hideAllDecorations() }
                }
            }
            // THE LAYOUT'S EAR ON FULLSCREEN (see WindowState): "will", not
            // "did", both ways, so the rail re-shapes as the transition starts
            // — under the system's own animation — instead of snapping after
            // it finishes.
            NotificationCenter.default.addObserver(
                forName: NSWindow.willEnterFullScreenNotification, object: window, queue: .main
            ) { _ in
                MainActor.assumeIsolated { WindowState.shared.isFullscreen = true }
            }
            NotificationCenter.default.addObserver(
                forName: NSWindow.willExitFullScreenNotification, object: window, queue: .main
            ) { _ in
                MainActor.assumeIsolated { WindowState.shared.isFullscreen = false }
            }
            // The BUTTONS ONLY on these, not the decoration walk with them: a
            // live resize fires continuously, and re-walking the frame view on
            // every frame of a drag to hide a view that is already hidden is a
            // cost for nothing. AppKit re-lays the titlebar out on both, which
            // is what puts the buttons back where it wants them.
            for name in [
                NSWindow.didResizeNotification,
                NSWindow.didEnterFullScreenNotification,
                NSWindow.didExitFullScreenNotification,
            ] {
                NotificationCenter.default.addObserver(
                    forName: name, object: window, queue: .main
                ) { _ in
                    MainActor.assumeIsolated {
                        for window in NSApp.windows { alignTrafficLights(in: window) }
                    }
                }
            }
        }

        /// Drop the traffic lights onto the top bar's line.
        ///
        /// AppKit centres them in the titlebar strip, which is shorter than the
        /// bar every page's header now sits in — so left alone the buttons ride
        /// a few points above the title beside them, and the top of the window
        /// reads as two rows of chrome that nearly line up rather than one that
        /// does. Only the origin moves: the buttons keep their own size, spacing
        /// and hit targets, so nothing about how they behave changes.
        ///
        /// IDEMPOTENT BY MEASUREMENT, not by a flag — it re-runs on every resize
        /// and focus change, and a nudge that applied a delta each time would
        /// walk the buttons off the bottom of the window.
        @MainActor
        private static func alignTrafficLights(in window: NSWindow) {
            // Fullscreen hands the buttons to the menu-bar overlay, which is the
            // system's bar at the system's height. Ours is not on screen to line
            // up with, and moving them there would only misalign them.
            guard !window.styleMask.contains(.fullScreen) else { return }
            for type in [NSWindow.ButtonType.closeButton, .miniaturizeButton, .zoomButton] {
                guard let button = window.standardWindowButton(type),
                    let container = button.superview
                else { continue }
                // The titlebar view is flush with the window's top edge, so a
                // depth measured against ITS top is a depth into the window. Its
                // coordinates are bottom-up, hence the subtraction both ways.
                let depth = container.bounds.maxY - button.frame.midY
                let drop = TopBar.height / 2 - depth
                guard abs(drop) > 0.5 else { continue }
                button.setFrameOrigin(
                    NSPoint(x: button.frame.minX, y: button.frame.minY - drop))
            }
        }

        /// Re-walk every window. NOTHING IS CAPTURED, deliberately: these closures
        /// are `@Sendable` and neither NSWindow nor Notification is Sendable, so
        /// reaching the windows through NSApp at call time is what satisfies the
        /// concurrency checker without an unchecked-Sendable box. The app has one or
        /// two windows and this runs only on focus/occlusion changes.
        @MainActor
        private static func hideAllDecorations() {
            for window in NSApp.windows {
                hideDecoration(in: window)
                alignTrafficLights(in: window)
            }
        }

        /// One walk of the frame view, hiding any decoration view it finds.
        @MainActor
        private static func hideDecoration(in window: NSWindow) {
            func walk(_ view: NSView) {
                if String(describing: type(of: view)).contains("TitlebarDecoration") {
                    view.isHidden = true
                }
                view.subviews.forEach(walk)
            }
            if let frameView = window.contentView?.superview { walk(frameView) }
        }

        func updateNSView(_ nsView: NSView, context: Context) {}
    }
#endif
