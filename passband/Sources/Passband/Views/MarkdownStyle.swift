// SPAN KIND → ATTRIBUTES. The one place the markdown scanner's output becomes
// something a text view can draw, shared by the live editor on BOTH platforms
// (AppKit's NSTextView, UIKit's UITextView) and by the review preview — so
// "what you typed", "what you are about to send" and "what the phone shows"
// cannot drift into three interpretations of the same source.
//
// This file was the second half of Views/MarkdownTextView.swift and is a MOVE,
// not a rewrite: the desktop's rendering is unchanged, and the only edits are
// the ones the shim demands. `NSAttributedString.Key` is cross-platform
// already; the fonts and colors behind it are not, so they go through
// Platform.swift's `PlatformFont` / `PlatformColor`.
//
// The one genuine fork is symbolic traits. AppKit spells them `.bold` /
// `.italic`, hands back a non-optional descriptor and an optional font; UIKit
// spells them `.traitBold` / `.traitItalic` and does exactly the opposite.
// `fontAdding` is that fork, and it is the only `#if` below the size table.
//
// THE SIZES ARE PER-PLATFORM ON PURPOSE. 13pt is a Mac's dense interface text;
// a phone's body copy is 17. Same ladder — heading over heading over base, code
// a half-step under it — moved up to the platform it is read on, because a
// composer rendering at 13pt on a 393pt screen is a squint, not a design.

import SwiftUI

#if os(macOS)
    import AppKit
#else
    import UIKit
#endif

@MainActor
enum MarkdownStyle {
    // MARK: - the ladder

    #if os(macOS)
        static let baseSize: CGFloat = 13
        static let codeSize: CGFloat = 12
        /// h1, h2, h3-and-deeper.
        static let headingSizes: (CGFloat, CGFloat, CGFloat) = (17, 15, 13.5)
    #else
        static let baseSize: CGFloat = 17
        static let codeSize: CGFloat = 15.5
        static let headingSizes: (CGFloat, CGFloat, CGFloat) = (22, 19.5, 17.5)
    #endif

    static let baseFont = PlatformFont.systemFont(ofSize: baseSize)

    static var base: [NSAttributedString.Key: Any] {
        [
            .font: baseFont,
            .foregroundColor: PlatformColor(Palette.ink),
        ]
    }

    // MARK: - the pass

    static func apply(_ span: MarkdownSpan, to storage: NSMutableAttributedString) {
        let range = span.range
        switch span.kind {
        case .bold:
            addTrait(.bold, storage, range)
        case .italic:
            addTrait(.italic, storage, range)
        case .code, .codeBlock:
            storage.addAttributes(
                [
                    .font: PlatformFont.monospacedSystemFont(ofSize: codeSize, weight: .regular),
                    .backgroundColor: PlatformColor(Palette.hairline).withAlphaComponent(0.5),
                ], range: range)
        case .heading(let level):
            let size =
                level == 1 ? headingSizes.0 : (level == 2 ? headingSizes.1 : headingSizes.2)
            storage.addAttribute(
                .font, value: PlatformFont.systemFont(ofSize: size, weight: .semibold), range: range
            )
        case .listMarker, .blockquoteMarker:
            storage.addAttribute(.foregroundColor, value: PlatformColor(Palette.accent), range: range)
        case .linkText:
            storage.addAttributes(
                [
                    .foregroundColor: PlatformColor(Palette.accent),
                    .underlineStyle: NSUnderlineStyle.single.rawValue,
                ], range: range)
        case .linkURL:
            storage.addAttribute(
                .foregroundColor, value: PlatformColor(Palette.inkFaintest), range: range)
        case .marker:
            storage.addAttribute(
                .foregroundColor, value: PlatformColor(Palette.inkFaint), range: range)
        }
    }

    /// The whole styled body as one attributed string — the review preview.
    ///
    /// The live editors do NOT go through here: they run the same loop over
    /// their own `textStorage`, adding attributes rather than replacing the
    /// string, because replacing it would throw away the selection and the undo
    /// stack on every keystroke.
    static func attributed(_ text: String) -> AttributedString {
        let styled = NSMutableAttributedString(string: text, attributes: base)
        for span in Markdown.spans(of: text) {
            guard span.range.location + span.range.length <= styled.length else { continue }
            apply(span, to: styled)
        }
        return AttributedString(styled)
    }

    // MARK: - traits

    /// Which weight/slant is being layered on. Named here rather than taken as a
    /// platform trait mask so callers never spell `.bold` vs `.traitBold`.
    enum Trait { case bold, italic }

    /// Layer a trait onto whatever font the range already carries, so bold inside
    /// a heading stays heading-sized.
    private static func addTrait(
        _ trait: Trait, _ storage: NSMutableAttributedString, _ range: NSRange
    ) {
        storage.enumerateAttribute(.font, in: range) { value, sub, _ in
            let current = (value as? PlatformFont) ?? baseFont
            storage.addAttribute(.font, value: fontAdding(trait, to: current), range: sub)
        }
    }

    /// THE fork. Both branches mean "this font, plus that trait, same size"; they
    /// disagree only about which spelling and which half of the pair is optional.
    private static func fontAdding(_ trait: Trait, to font: PlatformFont) -> PlatformFont {
        #if os(macOS)
            let wanted: NSFontDescriptor.SymbolicTraits = trait == .bold ? .bold : .italic
            let descriptor = font.fontDescriptor.withSymbolicTraits(
                font.fontDescriptor.symbolicTraits.union(wanted))
            return NSFont(descriptor: descriptor, size: font.pointSize) ?? font
        #else
            let wanted: UIFontDescriptor.SymbolicTraits = trait == .bold ? .traitBold : .traitItalic
            guard
                let descriptor = font.fontDescriptor.withSymbolicTraits(
                    font.fontDescriptor.symbolicTraits.union(wanted))
            else { return font }
            return UIFont(descriptor: descriptor, size: font.pointSize)
        #endif
    }
}
