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
    /// Files dropped ON THE EDITOR, with the UTF-16 offset of the drop point,
    /// so a picture lands where it was let go rather than at the end. nil
    /// leaves AppKit's own file-drop behaviour (which pastes the path).
    var onDropFiles: (([URL], Int?) -> Void)? = nil
    /// A picture pasted with ⌘V — a screenshot, a copied image — as PNG bytes
    /// with the caret's offset. Text pastes are untouched.
    var onPasteImage: ((Data, Int?) -> Void)? = nil

    func makeNSView(context: Context) -> NSScrollView {
        let view = HighlightingTextView()
        view.delegate = context.coordinator
        view.onDropFiles = onDropFiles
        view.onPasteImage = onPasteImage
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
        // Fresh closures every render, like the binding.
        view.onDropFiles = onDropFiles
        view.onPasteImage = onPasteImage
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
    var onDropFiles: (([URL], Int?) -> Void)?
    var onPasteImage: ((Data, Int?) -> Void)?

    // MARK: - files in

    // THE EDITOR IS A DROP TARGET FOR FILES, and it has to be this view that
    // says so: AppKit delivers a drag to the deepest view under the pointer,
    // and NSTextView already accepts file URLs — by inserting their PATHS as
    // text, which is never what dropping a photo on a mail means. So the drag
    // is claimed here when there is a handler and a file, and handed up with
    // the character the pointer is over; everything else falls through to
    // AppKit (text drags still work).

    private func hasFiles(_ info: NSDraggingInfo) -> Bool {
        onDropFiles != nil && !ComposeDrop.fileURLs(on: info.draggingPasteboard).isEmpty
    }

    override func draggingEntered(_ sender: NSDraggingInfo) -> NSDragOperation {
        hasFiles(sender) ? .copy : super.draggingEntered(sender)
    }

    override func draggingUpdated(_ sender: NSDraggingInfo) -> NSDragOperation {
        hasFiles(sender) ? .copy : super.draggingUpdated(sender)
    }

    override func performDragOperation(_ sender: NSDraggingInfo) -> Bool {
        guard let onDropFiles, hasFiles(sender) else {
            return super.performDragOperation(sender)
        }
        let urls = ComposeDrop.fileURLs(on: sender.draggingPasteboard)
        let point = convert(sender.draggingLocation, from: nil)
        onDropFiles(urls, characterIndexForInsertion(at: point))
        return true
    }

    /// ⌘V with a picture on the clipboard attaches it; ⌘V with a file copied
    /// in the Finder attaches that. Anything with text in it is a text paste,
    /// as ever — see `ComposeDrop.imagePNG`.
    override func paste(_ sender: Any?) {
        let pasteboard = NSPasteboard.general
        let caret = selectedRange().location
        if let onDropFiles {
            let urls = ComposeDrop.fileURLs(on: pasteboard)
            if !urls.isEmpty, pasteboard.string(forType: .string) == nil {
                onDropFiles(urls, caret)
                return
            }
        }
        if let onPasteImage, let png = ComposeDrop.imagePNG(on: pasteboard) {
            onPasteImage(png, caret)
            return
        }
        super.paste(sender)
    }

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
