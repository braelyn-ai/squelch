// THE PHONE HALF of the live markdown editor — the UIKit twin of the Mac's
// Views/MarkdownTextView.swift, and nothing more than that. The scanner
// (Lib/Markdown.swift) and the span → attributes pass (Views/MarkdownStyle.swift)
// are literally the same code on both platforms; this file exists because
// NSViewRepresentable and UIViewRepresentable are two protocols. It declares
// `MarkdownTextView` under the Mac's exact name, so ComposePane and InlineReply
// never fork on which OS they are running.
//
// NO SCROLL VIEW WRAPPER. AppKit needs an NSScrollView around an NSTextView to
// scroll at all; a UITextView IS a scroll view, so `isScrollEnabled` is the
// whole story and the representable hands back the text view itself.
//
// THREE THINGS A PHONE ADDS, none of which the Mac needs:
//
//  * A KEYBOARD ROW. There is no ⌘B on a phone, and markdown markers typed by
//    thumb on the symbol plane is a tax on the one surface that has to feel
//    fluent. Four buttons wrap the selection in the markers the SHARED scanner
//    already understands — bold, italic, code, link — plus a key to put the
//    keyboard away, since a sheet full of text view has nothing to tap off onto.
//
//  * CODE-SPAN INPUT TRAITS. iOS autocapitalizes after a period and autocorrects
//    everything, which inside `` `fooBar` `` produces text the daemon will render
//    as code that is not the code you typed. The scanner already says where the
//    code spans are, so the caret entering one turns both off and leaving one
//    turns them back on. Only on a real transition: `reloadInputViews` mid-word
//    flickers the predictive bar.
//
//  * A FOCUS ANSWER. `MarkdownFocus` is the honest twin of the Mac's
//    `NSApp.keyWindow?.firstResponder is NSTextView`: the composers ask whether
//    the caret is in a body editor before deciding what Enter means, and UIKit
//    has no public "who is first responder" to ask. The editors report it
//    instead. See ComposePane.bodyHasFocus.

import SwiftUI
import UIKit

struct MarkdownTextView: UIViewRepresentable {
    @Binding var text: String
    /// Grab the cursor when the editor appears — and, on a phone, raise the
    /// keyboard with it. Same affordance as the Mac's: a reply must land the
    /// caret in the body rather than being a box you have to go tap.
    var autofocus = false
    var disabled = false

    func makeUIView(context: Context) -> HighlightingTextView {
        let view = HighlightingTextView()
        view.delegate = context.coordinator
        context.coordinator.view = view
        view.text = text
        view.font = MarkdownStyle.baseFont
        view.textColor = PlatformColor(Palette.ink)
        view.tintColor = PlatformColor(Palette.accent)
        view.backgroundColor = .clear
        view.typingAttributes = MarkdownStyle.base
        // Matches the Mac's NSSize(width: 2, height: 4); UIKit spends the
        // horizontal half on the line fragment padding instead.
        view.textContainerInset = UIEdgeInsets(top: 4, left: 0, bottom: 4, right: 0)
        view.textContainer.lineFragmentPadding = 2
        // The editor scrolls ITSELF. Its callers give it a fixed height (the
        // inline reply) or the rest of a sheet (the pane), and a text view that
        // grew instead would push the send buttons under the keyboard.
        view.isScrollEnabled = true
        view.alwaysBounceVertical = false
        // Curly quotes and en-dashes break markers, exactly as on the Mac.
        view.smartQuotesType = .no
        view.smartDashesType = .no
        view.smartInsertDeleteType = .no
        view.inputAccessoryView = context.coordinator.accessory()
        view.rehighlight()
        // A body can open non-empty (the seeded signature); the caret belongs at
        // the top, above it, not after it where setting the text parks it.
        view.selectedRange = NSRange(location: 0, length: 0)
        if autofocus {
            // The view is not in a window yet at make time; one hop later it is.
            DispatchQueue.main.async { view.becomeFirstResponder() }
        }
        return view
    }

    func updateUIView(_ view: HighlightingTextView, context: Context) {
        // The representable is a fresh value every render; the coordinator is
        // not. Without this the delegate writes into the first render's binding.
        context.coordinator.parent = self
        view.isEditable = !disabled
        // Only external changes (a draft restore) land here — the coordinator
        // wrote user edits into the binding already, and re-setting the text for
        // those would throw away the selection.
        if view.text != text {
            view.text = text
            view.typingAttributes = MarkdownStyle.base
            view.rehighlight()
            // Same rule as mount: an externally landed body (draft restore)
            // starts reading — and typing — from the top.
            view.selectedRange = NSRange(location: 0, length: 0)
            view.scrollRangeToVisible(NSRange(location: 0, length: 0))
        }
    }

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    final class Coordinator: NSObject, UITextViewDelegate {
        var parent: MarkdownTextView
        weak var view: HighlightingTextView?

        init(_ parent: MarkdownTextView) { self.parent = parent }

        func textViewDidChange(_ textView: UITextView) {
            guard let view = textView as? HighlightingTextView else { return }
            parent.text = view.text
            view.rehighlight()
            view.syncInputTraits()
        }

        func textViewDidChangeSelection(_ textView: UITextView) {
            (textView as? HighlightingTextView)?.syncInputTraits()
        }

        func textViewDidBeginEditing(_ textView: UITextView) {
            MarkdownFocus.shared.began()
        }

        func textViewDidEndEditing(_ textView: UITextView) {
            MarkdownFocus.shared.ended()
        }

        // MARK: - the keyboard row

        /// Four markers and a way out. Deliberately NOT a formatting model: each
        /// button types the same characters a keyboard would, into the same
        /// source the scanner reads, so there is no second representation of a
        /// document that has only ever been markdown text.
        func accessory() -> UIView {
            let bar = UIToolbar(frame: CGRect(x: 0, y: 0, width: 0, height: 44))
            bar.tintColor = PlatformColor(Palette.accent)
            let flexible = UIBarButtonItem(
                barButtonSystemItem: .flexibleSpace, target: nil, action: nil)
            bar.items = [
                item("bold", #selector(tapBold), "Bold"),
                item("italic", #selector(tapItalic), "Italic"),
                item("curlybraces", #selector(tapCode), "Code"),
                item("link", #selector(tapLink), "Link"),
                flexible,
                item("keyboard.chevron.compact.down", #selector(tapDismiss), "Hide keyboard"),
            ]
            bar.sizeToFit()
            return bar
        }

        private func item(_ symbol: String, _ action: Selector, _ label: String) -> UIBarButtonItem {
            let button = UIBarButtonItem(
                image: UIImage(systemName: symbol), style: .plain, target: self, action: action)
            button.accessibilityLabel = label
            return button
        }

        @objc private func tapBold() { wrap("**") }
        @objc private func tapItalic() { wrap("*") }
        @objc private func tapCode() { wrap("`") }
        @objc private func tapDismiss() { view?.resignFirstResponder() }

        /// `[selection](url)`, with `url` left selected so the next keystrokes
        /// replace it. An empty selection gets the same skeleton with the link
        /// TEXT selected instead, because that is what you type first.
        @objc private func tapLink() {
            guard let view else { return }
            let range = view.selectedRange
            let selected = (view.text as NSString).substring(with: range)
            let inner = selected.isEmpty ? "text" : selected
            guard replace(range, with: "[\(inner)](url)") else { return }
            let placeholder =
                selected.isEmpty
                ? NSRange(location: range.location + 1, length: (inner as NSString).length)
                : NSRange(
                    location: range.location + 1 + (inner as NSString).length + 2, length: 3)
            view.selectedRange = placeholder
        }

        /// Put `marker` on both sides of the selection. Empty selection leaves the
        /// caret between the two markers; a real one stays selected, so a second
        /// tap layers italic onto bold the way it reads.
        private func wrap(_ marker: String) {
            guard let view else { return }
            let range = view.selectedRange
            let selected = (view.text as NSString).substring(with: range)
            guard replace(range, with: marker + selected + marker) else { return }
            let markerLength = (marker as NSString).length
            view.selectedRange = NSRange(
                location: range.location + markerLength, length: range.length)
        }

        /// Edit THROUGH UITextInput rather than assigning `text`, so the system
        /// undo stack keeps the edit and `textViewDidChange` still fires — which
        /// is what carries the change into the binding and arms the autosave.
        private func replace(_ range: NSRange, with string: String) -> Bool {
            guard let view,
                let start = view.position(from: view.beginningOfDocument, offset: range.location),
                let end = view.position(from: start, offset: range.length),
                let textRange = view.textRange(from: start, to: end)
            else { return false }
            view.replace(textRange, withText: string)
            return true
        }
    }
}

// MARK: - the text view

/// The UITextView half: owns the re-style pass. Attribute changes never touch
/// the characters, so the selection and the undo stack survive every pass.
final class HighlightingTextView: UITextView {
    /// The last scan, kept so a bare caret move can ask where it is without
    /// re-scanning the document.
    private var spans: [MarkdownSpan] = []

    func rehighlight() {
        let source = text ?? ""
        spans = Markdown.spans(of: source)
        let storage = textStorage
        let all = NSRange(location: 0, length: storage.length)
        storage.beginEditing()
        storage.setAttributes(MarkdownStyle.base, range: all)
        for span in spans {
            guard span.range.location + span.range.length <= storage.length else { continue }
            MarkdownStyle.apply(span, to: storage)
        }
        storage.endEditing()
        // setAttributes does not touch what the NEXT character will be typed
        // with, and UIKit resets typing attributes to the view's font on some
        // edits; restating them keeps a fresh keystroke from landing unstyled.
        typingAttributes = MarkdownStyle.base
    }

    /// Autocapitalize and autocorrect prose; do neither inside a code span. See
    /// the file header — the guard is what keeps this off the fast path.
    func syncInputTraits() {
        let code = caretIsInCode
        let capitalization: UITextAutocapitalizationType = code ? .none : .sentences
        let correction: UITextAutocorrectionType = code ? .no : .default
        guard autocapitalizationType != capitalization || autocorrectionType != correction
        else { return }
        autocapitalizationType = capitalization
        autocorrectionType = correction
        if isFirstResponder { reloadInputViews() }
    }

    /// Inclusive of both ends: typing at the closing backtick is still typing
    /// code, and the span the caret is about to extend is the one that counts.
    private var caretIsInCode: Bool {
        let caret = selectedRange.location
        return spans.contains { span in
            guard span.kind == .code || span.kind == .codeBlock else { return false }
            return caret >= span.range.location && caret <= span.range.location + span.range.length
        }
    }

    // Palette colors are appearance-dynamic providers, but font CHOICES made
    // per-span are not — restyle when the theme flips so nothing goes stale.
    override func traitCollectionDidChange(_ previous: UITraitCollection?) {
        super.traitCollectionDidChange(previous)
        guard traitCollection.userInterfaceStyle != previous?.userInterfaceStyle else { return }
        rehighlight()
    }
}

// MARK: - focus

/// WHO HAS THE CARET. The Mac reads `NSApp.keyWindow?.firstResponder`; UIKit
/// publishes no such thing, so the editors report themselves instead and the
/// composers ask here. A COUNT rather than a flag because two editors can be
/// mounted at once (a compose sheet over a thread holding an inline reply) and
/// the second one's `began` must not be undone by the first one's `ended`.
@MainActor
final class MarkdownFocus {
    static let shared = MarkdownFocus()

    private var editing = 0

    private init() {}

    /// True while ANY markdown body editor is first responder.
    var isEditing: Bool { editing > 0 }

    func began() { editing += 1 }
    func ended() { editing = max(0, editing - 1) }
}
