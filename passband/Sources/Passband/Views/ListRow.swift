// The selectable list-row envelope: the Button/hover/selection sandwich every
// dense list on this app was hand-rolling. A row supplies its guts; this owns
// the click target, the hover state and the selection paint, so the six lists
// can no longer drift apart on padding, hit shape or fill.
//
// It lives here rather than in Design/Chrome.swift on purpose: everything in
// that file is a LEAF, and swapping one in can never change the identity of the
// row around it. This one WRAPS a subtree and holds `@State`, so it belongs
// beside the rows it envelopes.
//
// SELECTION IS ONE IDENTITY, DELIBERATELY — `selectionFill` is unconditional
// (no `if selected` branch anywhere in here) precisely so its alphas
// interpolate instead of tearing the row's subtree down; see selectionFill's
// own doc in Design/Glass.swift for what that branch used to cost.

import SwiftUI

struct ListRow<Content: View>: View {
    let selected: Bool
    /// Matches the row's selection paint; each list states its own.
    var cornerRadius: CGFloat = 9
    /// The selection tint — semantic per list (auth is `lock`, browse is the
    /// tier colour, triage targets are the axis colour).
    var tint: Color = Palette.accent
    var hPadding: CGFloat = 11
    var vPadding: CGFloat = 6
    /// Whether the pointer paints a hover wash. Off for lists whose selection
    /// already follows the pointer (a second wash under the first reads as a
    /// smear) — and with it off, `hovering` is never written, so a mouse sweep
    /// over the list invalidates nothing.
    var hoverFill: Bool = true
    /// Hover bookkeeping the row's OWNER needs — moving a cursor to the pointer,
    /// so a keyboard action fires against the row under the mouse. Fires after
    /// the local hover state is set.
    var onHoverChange: ((Bool) -> Void)?
    let action: () -> Void
    /// Guts, given `selected` and `hovering`: several rows change what they show
    /// on either (key hints on the selected row, an unclamped line count).
    let content: (_ selected: Bool, _ hovering: Bool) -> Content

    @State private var hovering = false

    init(
        selected: Bool,
        cornerRadius: CGFloat = 9,
        tint: Color = Palette.accent,
        hPadding: CGFloat = 11,
        vPadding: CGFloat = 6,
        hoverFill: Bool = true,
        onHoverChange: ((Bool) -> Void)? = nil,
        action: @escaping () -> Void,
        @ViewBuilder content: @escaping (_ selected: Bool, _ hovering: Bool) -> Content
    ) {
        self.selected = selected
        self.cornerRadius = cornerRadius
        self.tint = tint
        self.hPadding = hPadding
        self.vPadding = vPadding
        self.hoverFill = hoverFill
        self.onHoverChange = onHoverChange
        self.action = action
        self.content = content
    }

    /// No tracking area at all for a row that neither washes on hover nor
    /// reports it — an `.onHover` whose closure does nothing still costs an
    /// enter/exit event per row per sweep. The flags are constants per call
    /// site, so this branch is decided once and never flips.
    private var tracksHover: Bool { hoverFill || onHoverChange != nil }

    var body: some View {
        let row = Button(action: action) {
            content(selected, hovering)
                .padding(.horizontal, hPadding)
                .padding(.vertical, vPadding)
                // Without this only the glyphs are clickable, so the gaps
                // between a row's columns fall through to whatever is under it.
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .selectionFill(selected, hovering: hovering, tint: tint, cornerRadius: cornerRadius)

        if tracksHover {
            row.onHover { over in
                if hoverFill { hovering = over }
                onHoverChange?(over)
            }
        } else {
            row
        }
    }
}
