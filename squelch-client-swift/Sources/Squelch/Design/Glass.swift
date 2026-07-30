// THE GLASS LAYER — genuine Liquid Glass, not a translucent rectangle.
//
// What makes this different from the CSS version it replaces: `.glassEffect`
// is an AppKit-backed material that samples the REAL window backdrop (the
// desktop behind a non-opaque window, and page content it overlaps), refracts
// it, and draws its own specular edge. CSS `backdrop-filter` cannot see outside
// the page, which is why every "glass" card in the web build was a hand-drawn
// translucent box with a fake hairline.
//
// Three things this file establishes:
//
//  1. TINTED GLASS CARRIES THE BRAND. `Glass.regular.tint(...)` pushes squelch
//     blue into the material itself rather than painting a blue rectangle on
//     top of a grey one. Semantic surfaces (overdue, auth, positive) tint with
//     their tier color, so the material states the meaning.
//
//  2. MERGING IS THE SIGNATURE. Adjacent glass inside a `GlassEffectContainer`
//     flows together and separates as it moves — a behavior with no web
//     equivalent at all. The sitrep zones, the rail, the right-hand record
//     cards and the toast stack are all containers for exactly that reason.
//
//  3. MATCHED-GEOMETRY GLASS. `glassEffectID(_:in:)` with a `@Namespace` lets
//     one glass shape morph into another across a state change — the sitrep's
//     "N more" expander and the palette/askbar entrances use it, so the
//     material appears to STRETCH rather than cross-fade.

import AppKit
import SwiftUI

// MARK: - surface vocabulary

/// How much presence a glass surface has. Maps onto the two real materials
/// (`.regular` / `.clear`) plus a tint, rather than inventing densities.
enum GlassLevel {
    /// A primary content pane: a zone card, a modal, the reader chrome.
    case pane
    /// A surface nested INSIDE a pane (a nested well, a record row).
    case nested
    /// Chrome that must stay maximally see-through: the rail, chips.
    case chrome
}

extension View {
    /// The app's standard glass card. `tint` defaults to the squelch wash so
    /// panes read as *ours* — pass a tier color for semantic surfaces, or
    /// `.clear`-ish nil for a neutral pane.
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

    /// THE SELECTION MATERIAL for a list row.
    ///
    /// Only the selected row carries glass, and every row in a list shares ONE
    /// `glassEffectID`, so moving the cursor makes the material physically FLOW
    /// from row to row inside the list's `GlassEffectContainer` rather than
    /// fading out here and in there. That travel is the single clearest
    /// demonstration of the real material in a dense list — and it is precisely
    /// what a CSS `background-color` swap cannot do.
    ///
    /// The active-item material for a small selector (the rail).
    ///
    /// Applied to the CONTENT, never as a sibling background: a
    /// `GlassEffectContainer` deliberately raises its descendants above its
    /// content view, so glass added behind an icon renders on top of it.
    ///
    /// UNCONDITIONAL, and that is the whole design of it. Written as
    /// `if active { self.glassEffect(…) } else { self }` the two branches are
    /// separate view identities, so every selection change TORE DOWN the icon's
    /// subtree and built a new one — visible as a flicker on the item being
    /// deselected, and fatal to the motion: a `glassEffectID` cannot be matched
    /// across a view that no longer exists, so the material popped between icons
    /// instead of travelling. `Glass.identity` is the inactive state (the
    /// modifier stays applied and renders nothing) and the shared id is handed
    /// over via the OPTIONAL `glassEffectID`, so exactly one icon claims the
    /// selector at a time and the container animates it from the old owner to
    /// the new one.
    func glassSelector(
        _ active: Bool,
        tint: Color = Palette.accent,
        cornerRadius: CGFloat = 11,
        id: String,
        in namespace: Namespace.ID
    ) -> some View {
        self.glassEffect(
            active ? .regular.tint(tint.opacity(0.26)).interactive() : .identity,
            in: .rect(cornerRadius: cornerRadius, style: .continuous)
        )
        .glassEffectID(active ? id : nil, in: namespace)
    }

    /// Unselected rows get a cheap hover wash instead: putting glass on 500
    /// simultaneous rows would be both illegible and slow.
    ///
    /// COST OF THE BRANCH, and why both lists gate the glass on `kbActive`:
    /// this is a conditional modifier, so the two branches are separate view
    /// identities and flipping `selected` re-creates the row's ENTIRE subtree —
    /// `@State` reset, `.task` re-run, layout rebuilt. Fine a few times per
    /// keypress; ruinous if it happens twice for every row a mouse crosses,
    /// which is what made sitrep hover trail the cursor (fixed 2026-07-27).
    ///
    /// Moving the glass into a `.background` layer would keep row identity
    /// stable, and does render correctly on its own — but NOT inside a
    /// `GlassEffectContainer`, which hoists glass above the container's content
    /// and so draws the material over the row's own text. Measured, not assumed.
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

/// A sitrep zone: engraved heading with a glyph + count, then content, on one
/// tinted glass pane. Every zone in the dashboard is one of these so they merge
/// coherently inside their container.
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
                    // Pin the casing: SwiftUI's textCase is an ENVIRONMENT
                    // value, so an ancestor (or a future container) could
                    // silently uppercase every zone heading. Zone titles are
                    // sentence case by design — the engraved uppercase voice
                    // belongs to Settings/Usage section labels, not here.
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

/// A small status chip. Tier chips (overdue/deadline) tint their glass with the
/// tier color so the material itself carries the semantics.
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
/// glass card centered (or top-anchored, for command bars) above it.
///
/// Matches the desktop client's overlay contract exactly — conditional-mount by
/// the parent, own "modal" KeyContext, Esc + backdrop-click to close.
struct OverlayScrim<Content: View>: View {
    var alignment: Alignment = .center
    var topInset: CGFloat = 0
    let onDismiss: () -> Void
    @ViewBuilder var content: Content

    var body: some View {
        ZStack(alignment: alignment) {
            // DIM ONLY — the defocus is done by BLURRING THE CONTENT ITSELF
            // (see MainShell.modalBlur), not by stacking a material here.
            // A material is not a blur: `.thinMaterial` over the dark board
            // flattened it into an opaque slab and `.ultraThinMaterial` read as
            // nothing at all. Neither one keeps the app visible, which is the
            // whole point — you should still see the board you're asking about.
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
/// tint over it.
///
/// A non-opaque window over a `.underWindowBackground` visual-effect view is
/// what lets `.glassEffect` sample real desktop content: without it the window
/// is either an opaque slab (glass with nothing to refract) or a literal hole.
/// This is the direct analogue of `apply_native_backdrop` in the Tauri shell,
/// except here the glass ABOVE it is native too.
///
/// THE SCRIM is not decoration and it is not optional. It is tuned against the
/// HARD case — a dark, busy wallpaper behind a light-theme window — because
/// below roughly this alpha whatever sits behind bleeds through hard enough
/// that light mode reads as a muddy dark one with white cards floating on it.
/// It is still a material, not a fill: blurred wallpaper shape and colour read
/// through clearly at this alpha, and the glass above it still refracts.
struct WindowBackdrop: View {
    var body: some View {
        ZStack {
            VisualEffectBackdrop()
            // The canvas is BLUE, not neutral — this is the squelch window, and
            // a grey one reads as any other app. Light mode is a pale sky that
            // deepens toward the bottom; dark mode is a blue-black rather than
            // a true black, so the accent still has somewhere to sit.
            LinearGradient(
                colors: [
                    Color(light: 0xE6EFFC, dark: 0x0C1420).opacity(0.66),
                    Color(light: 0xD5E5F8, dark: 0x080E19).opacity(0.72),
                ],
                startPoint: .top, endPoint: .bottom)
            // The brand wash on top of it, so even the empty parts of the
            // window belong to squelch rather than to macOS.
            Palette.glassTint.opacity(0.55)
        }
    }
}

// MARK: - text actions

/// An action that is nothing but its own label — no pill, no border, no fill.
/// Hover IS the affordance.
///
/// For chrome that sits directly above content the user is reading: a row of
/// outlined glass pills up there reads as a toolbar demanding attention, and
/// competes with the mail it is framing. The rest color is deliberately faint
/// enough to recede and the hover color is the accent, because with no shape
/// of its own the color change is the only thing left to say "this is a
/// button". Wrap key hints (`Kbd`) in the label as usual; they keep their own
/// chip, which is the point — the hint is the affordance at rest.
struct TextActionStyle: ButtonStyle {
    @State private var hovering = false

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .foregroundStyle(hovering ? Palette.accent : Palette.inkFaint)
            .opacity(configuration.isPressed ? 0.55 : 1)
            // Without a background the label's glyphs would be the only hit
            // target, so hover would flicker between letters.
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
        // Stay frosted when the window loses focus — the sitrep is a glanceable
        // surface, so it has to stay legible sitting in the background.
        view.state = .active
        return view
    }

    func updateNSView(_ nsView: NSVisualEffectView, context: Context) {}
}

/// Reaches the hosting NSWindow once, to make it non-opaque (so the backdrop
/// reads) and to hide the title bar while keeping the traffic lights.
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
        }
        return view
    }

    func updateNSView(_ nsView: NSView, context: Context) {}
}
