// The live markdown editor: an NSTextView that re-styles itself on every edit
// from `Markdown.spans`, with the syntax markers kept visible — `**asdf**`
// shows its stars and reads bold. An NSViewRepresentable rather than a
// TextEditor because live re-attribution is NSTextStorage's home turf, and the
// key monitor's input guard already speaks NSTextView (see KeyMonitor.isEditing).
//
// The contract with the composers: it binds the SAME Binding<String> the plain
// TextEditor did — every keystroke lands in ComposeState through the caller's
// setter, so the DraftSaver hook there keeps arming autosaves. Plain Enter
// stays a newline (the ceremony's Enter binding declines in edit phase and the
// event falls through to this view).

import AppKit
import SwiftUI

struct MarkdownTextView: NSViewRepresentable {
    @Binding var text: String
    /// Grab the cursor when the editor appears. Same affordance as the plain
    /// editors this replaces: `r` must land the caret in the body.
    var autofocus = false
    var disabled = false

    func makeNSView(context: Context) -> NSScrollView {
        let view = HighlightingTextView()
        view.delegate = context.coordinator
        view.string = text
        view.allowsUndo = true
        view.isRichText = false  // attributes are OURS; no pasted fonts
        view.font = MarkdownStyle.baseFont
        view.textColor = NSColor(Palette.ink)
        view.insertionPointColor = NSColor(Palette.accent)
        view.drawsBackground = false
        view.textContainerInset = NSSize(width: 2, height: 4)
        view.isAutomaticQuoteSubstitutionEnabled = false  // curly quotes break markers
        view.isAutomaticDashSubstitutionEnabled = false
        view.autoresizingMask = [.width]
        view.textContainer?.widthTracksTextView = true
        view.rehighlight()

        let scroll = NSScrollView()
        scroll.documentView = view
        scroll.hasVerticalScroller = true
        scroll.drawsBackground = false
        scroll.verticalScrollElasticity = .automatic

        if autofocus {
            // The window exists only after mount; one hop later is soon enough.
            DispatchQueue.main.async { view.window?.makeFirstResponder(view) }
        }
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        guard let view = scroll.documentView as? HighlightingTextView else { return }
        // The representable is a fresh value every render; the coordinator is
        // not. Without this the delegate writes into the first render's binding.
        context.coordinator.parent = self
        view.isEditable = !disabled
        // Only external changes (a draft restore) land here — the coordinator
        // wrote user edits into the binding already, and re-setting the string
        // for those would throw away the selection.
        if view.string != text {
            view.string = text
            view.rehighlight()
        }
    }

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    final class Coordinator: NSObject, NSTextViewDelegate {
        var parent: MarkdownTextView
        init(_ parent: MarkdownTextView) { self.parent = parent }

        func textDidChange(_ notification: Notification) {
            guard let view = notification.object as? HighlightingTextView else { return }
            parent.text = view.string
            view.rehighlight()
        }
    }
}

/// The NSTextView half: owns the re-style pass. Attribute changes never touch
/// the characters, so the selection and undo stack survive every pass.
final class HighlightingTextView: NSTextView {
    func rehighlight() {
        guard let storage = textStorage else { return }
        let all = NSRange(location: 0, length: storage.length)
        storage.beginEditing()
        storage.setAttributes(MarkdownStyle.base, range: all)
        for span in Markdown.spans(of: string) {
            guard span.range.location + span.range.length <= storage.length else { continue }
            MarkdownStyle.apply(span, to: storage)
        }
        storage.endEditing()
    }

    // Palette colors are appearance-dynamic providers, but font CHOICES made
    // per-span are not — restyle when the theme flips so nothing goes stale.
    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        rehighlight()
    }
}

/// Span kind → NSAttributedString attributes, shared by the live editor and
/// the review preview so "what you typed" and "what you're about to send"
/// cannot drift apart.
@MainActor
enum MarkdownStyle {
    static let baseFont = NSFont.systemFont(ofSize: 13)

    static var base: [NSAttributedString.Key: Any] {
        [
            .font: baseFont,
            .foregroundColor: NSColor(Palette.ink),
        ]
    }

    static func apply(_ span: MarkdownSpan, to storage: NSMutableAttributedString) {
        let range = span.range
        switch span.kind {
        case .bold:
            addTraits(.bold, storage, range)
        case .italic:
            addTraits(.italic, storage, range)
        case .code, .codeBlock:
            storage.addAttributes(
                [
                    .font: NSFont.monospacedSystemFont(ofSize: 12, weight: .regular),
                    .backgroundColor: NSColor(Palette.hairline).withAlphaComponent(0.5),
                ], range: range)
        case .heading(let level):
            let size: CGFloat = level == 1 ? 17 : (level == 2 ? 15 : 13.5)
            storage.addAttribute(
                .font, value: NSFont.systemFont(ofSize: size, weight: .semibold), range: range)
        case .listMarker, .blockquoteMarker:
            storage.addAttribute(.foregroundColor, value: NSColor(Palette.accent), range: range)
        case .linkText:
            storage.addAttributes(
                [
                    .foregroundColor: NSColor(Palette.accent),
                    .underlineStyle: NSUnderlineStyle.single.rawValue,
                ], range: range)
        case .linkURL:
            storage.addAttribute(
                .foregroundColor, value: NSColor(Palette.inkFaintest), range: range)
        case .marker:
            storage.addAttribute(
                .foregroundColor, value: NSColor(Palette.inkFaint), range: range)
        }
    }

    /// The whole styled body as one attributed string — the review preview.
    static func attributed(_ text: String) -> AttributedString {
        let styled = NSMutableAttributedString(string: text, attributes: base)
        for span in Markdown.spans(of: text) {
            guard span.range.location + span.range.length <= styled.length else { continue }
            apply(span, to: styled)
        }
        return AttributedString(styled)
    }

    /// Layer a symbolic trait onto whatever font the range already carries, so
    /// bold inside a heading stays heading-sized.
    private static func addTraits(
        _ trait: NSFontDescriptor.SymbolicTraits, _ storage: NSMutableAttributedString,
        _ range: NSRange
    ) {
        storage.enumerateAttribute(.font, in: range) { value, sub, _ in
            let current = (value as? NSFont) ?? baseFont
            let descriptor = current.fontDescriptor.withSymbolicTraits(
                current.fontDescriptor.symbolicTraits.union(trait))
            let font = NSFont(descriptor: descriptor, size: current.pointSize) ?? current
            storage.addAttribute(.font, value: font, range: sub)
        }
    }
}
