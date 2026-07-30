// The glass layer. `.glassEffect` is an AppKit material that samples the REAL
// window backdrop, so it only reads over a non-opaque window (WindowBackdrop).
// Semantic surfaces tint the material itself rather than painting over it.
// Adjacent glass inside a `GlassEffectContainer` merges and separates as it
// moves; `glassEffectID(_:in:)` with a shared `@Namespace` morphs one shape
// into another instead of cross-fading.

import AppKit
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
    func squelchGlass(
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
    func squelchGlass<ID: Hashable & Sendable>(
        _ level: GlassLevel = .pane,
        cornerRadius: CGFloat = 16,
        tint: Color? = Palette.glassTint,
        interactive: Bool = false,
        id: ID,
        in namespace: Namespace.ID
    ) -> some View {
        squelchGlass(level, cornerRadius: cornerRadius, tint: tint, interactive: interactive)
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

    /// The active-item material for a small selector (the rail). Only the active
    /// item carries glass; items sharing one `glassEffectID` make the material
    /// FLOW between them rather than fade out here and in there.
    ///
    /// Applied to the CONTENT, never as a sibling background: a
    /// `GlassEffectContainer` deliberately raises its descendants above its
    /// content view, so glass added behind an icon renders on top of it.
    @ViewBuilder
    func glassSelector(
        _ active: Bool,
        tint: Color = Palette.accent,
        cornerRadius: CGFloat = 11,
        id: String,
        in namespace: Namespace.ID
    ) -> some View {
        if active {
            self.glassEffect(
                .regular.tint(tint.opacity(0.26)).interactive(),
                in: .rect(cornerRadius: cornerRadius, style: .continuous)
            )
            .glassEffectID(id, in: namespace)
        } else {
            self
        }
    }

    /// The selection material for a list row. Only the selected row carries
    /// glass; unselected rows get a cheap hover wash, because glass on 500
    /// simultaneous rows would be both illegible and slow.
    ///
    /// COST OF THE BRANCH, and why both lists gate the glass on `kbActive`:
    /// this is a conditional modifier, so the two branches are separate view
    /// identities and flipping `selected` re-creates the row's ENTIRE subtree —
    /// `@State` reset, `.task` re-run, layout rebuilt. Fine a few times per
    /// keypress; ruinous twice for every row a mouse crosses. Moving the glass
    /// into a `.background` layer would keep row identity stable and renders
    /// correctly on its own, but NOT inside a `GlassEffectContainer`, which
    /// hoists glass above the container's content and so draws the material
    /// over the row's own text.
    @ViewBuilder
    func selectionGlass(
        _ selected: Bool,
        hovering: Bool = false,
        tint: Color = Palette.accent,
        cornerRadius: CGFloat = 9,
        id: String,
        in namespace: Namespace.ID
    ) -> some View {
        if selected {
            self.glassEffect(
                .regular.tint(tint.opacity(0.28)).interactive(),
                in: .rect(cornerRadius: cornerRadius, style: .continuous)
            )
            .glassEffectID(id, in: namespace)
        } else {
            self.background(
                RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
                    .fill(hovering ? Palette.hairline.opacity(0.55) : .clear)
            )
        }
    }
}

// MARK: - zone card

/// A sitrep zone: heading with a glyph + count, then content, on one tinted
/// glass pane.
struct ZoneCard<Content: View>: View {
    let symbol: String
    let title: String
    var count: Int?
    var subtitle: String?
    /// A semantic tint; nil uses the standard squelch wash.
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

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
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
                if let subtitle {
                    Text(subtitle)
                        .font(Typo.micro)
                        .foregroundStyle(Palette.inkFaintest)
                }
                Spacer(minLength: 4)
                if let trailing { trailing }
            }
            content
        }
        .zonePadding()
        .frame(maxWidth: .infinity, alignment: .leading)
        .squelchGlass(.pane, cornerRadius: 18, tint: tint?.opacity(0.12) ?? Palette.glassTint)
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
            .frame(width: width)
            .squelchGlass(.pane, cornerRadius: 20, tint: tint)
            .shadow(color: .black.opacity(0.28), radius: 40, y: 18)
    }
}

// MARK: - native window backdrop

/// The AppKit backdrop the whole glass language sits ON, plus the app's own
/// tint over it. A non-opaque window over a `.underWindowBackground` visual-
/// effect view is what lets `.glassEffect` sample real desktop content.
///
/// The scrim alphas are tuned against the hard case — a dark, busy wallpaper
/// behind a light-theme window — because much below these values the backdrop
/// bleeds through and light mode reads as a muddy dark one; much above and the
/// wallpaper stops reading through at all.
struct WindowBackdrop: View {
    var body: some View {
        ZStack {
            VisualEffectBackdrop()
            // The canvas is blue, not neutral: light mode a pale sky deepening
            // toward the bottom, dark mode a blue-black rather than a true
            // black, so the accent still has somewhere to sit.
            LinearGradient(
                colors: [
                    Color(light: 0xE6EFFC, dark: 0x0C1420).opacity(0.66),
                    Color(light: 0xD5E5F8, dark: 0x080E19).opacity(0.72),
                ],
                startPoint: .top, endPoint: .bottom)
            // The brand wash on top, so empty window area is squelch's too.
            Palette.glassTint.opacity(0.55)
        }
    }
}

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
        func walk(_ view: NSView) {
            if String(describing: type(of: view)).contains("TitlebarDecoration") {
                view.isHidden = true
            }
            view.subviews.forEach(walk)
        }
        func hide() {
            if let frameView = window.contentView?.superview { walk(frameView) }
        }
        hide()
        for delay in [0.25, 0.75, 2.0] {
            DispatchQueue.main.asyncAfter(deadline: .now() + delay) { hide() }
        }
        for name in [
            NSWindow.didBecomeKeyNotification, NSWindow.didResignKeyNotification,
            NSWindow.didBecomeMainNotification, NSWindow.didResignMainNotification,
            NSWindow.didChangeOcclusionStateNotification,
        ] {
            NotificationCenter.default.addObserver(
                forName: name, object: window, queue: .main
            ) { _ in
                MainActor.assumeIsolated { hide() }
            }
        }
    }

    func updateNSView(_ nsView: NSView, context: Context) {}
}
