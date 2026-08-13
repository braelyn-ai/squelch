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
//
// THE APPKIT HALF, AND ONLY THAT. The span → attributes pass lives in
// Views/MarkdownStyle.swift and is literally the same code on both platforms;
// the UIKit twin is Sources/PassbandiOS/Views/MarkdownTextViewiOS.swift, which
// declares `MarkdownTextView` under this exact name so no composer forks on
// which OS it is running.

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
        // A body can open non-empty (the seeded signature); the caret belongs
        // at the top, above it, not after it where setString parks it.
        view.setSelectedRange(NSRange(location: 0, length: 0))

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
            // Same rule as mount: an externally landed body (draft restore)
            // starts reading — and typing — from the top.
            view.setSelectedRange(NSRange(location: 0, length: 0))
            view.scrollRangeToVisible(NSRange(location: 0, length: 0))
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
