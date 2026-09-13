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
    /// A file is being dragged over the editor (true) or has left it (false),
    /// so the composer can draw the same border it draws for the rest of the
    /// pane — AppKit claims the drag here, and SwiftUI's `isTargeted` never
    /// hears about it.
    var onDropHover: ((Bool) -> Void)? = nil

    func makeNSView(context: Context) -> NSScrollView {
        let view = HighlightingTextView()
        view.delegate = context.coordinator
        view.onDropFiles = onDropFiles
        view.onPasteImage = onPasteImage
        view.onDropHover = onDropHover
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
        view.onDropHover = onDropHover
        // Only external changes (a draft restore) land here — the coordinator
        // wrote user edits into the binding already, and re-setting the string
        // for those would throw away the selection.
        if view.string != text {
            // TWO KINDS OF EXTERNAL CHANGE, and they want opposite carets. A
            // draft RESTORE lands a whole body into an untouched editor, and
            // reading starts from the top. A DROPPED PICTURE inserts one
            // marker into text somebody is typing, and the caret has to end
            // up after the insertion — sending it to the top would drop the
            // next keystroke above everything written so far.
            //
            // AND THEY WANT DIFFERENT MECHANICS. A restore may simply set the
            // string. An edit under a live editor goes through the text
            // system as a REPLACEMENT of the changed run, so it joins the
            // undo stack in order: `string =` bypasses the undo manager, and
            // the groups recorded before it keep ranges that no longer exist
            // — ⌘Z after a drop would then cut a marker in half, or throw
            // past the end of a body a tray click had shortened.
            if Prefs.shared.isBodyUntouched(view.string) {
                view.string = text
                view.rehighlight()
                view.setSelectedRange(NSRange(location: 0, length: 0))
                view.scrollRangeToVisible(NSRange(location: 0, length: 0))
            } else {
                view.replaceExternally(with: text)
            }
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
    var onDropHover: ((Bool) -> Void)?

    // MARK: - the binding writing under a live editor

    /// Land `text` as ONE replacement of the run that differs, through the
    /// text system, so the undo manager records it in sequence and the caret
    /// ends after the change (see `updateNSView`). The run is the smallest
    /// span between a common prefix and a common suffix; for the marker a
    /// drop inserts, that is the marker and its line breaks.
    func replaceExternally(with text: String) {
        let old = string
        let o = Array(old.utf16), n = Array(text.utf16)
        var prefix = 0
        while prefix < o.count, prefix < n.count, o[prefix] == n[prefix] { prefix += 1 }
        var suffix = 0
        while suffix < o.count - prefix, suffix < n.count - prefix,
            o[o.count - 1 - suffix] == n[n.count - 1 - suffix]
        {
            suffix += 1
        }
        let range = NSRange(location: prefix, length: o.count - prefix - suffix)
        let replacement = String(utf16CodeUnits: Array(n[prefix..<(n.count - suffix)]), count: n.count - prefix - suffix)
        let caret = selectedRange().location
        guard shouldChangeText(in: range, replacementString: replacement) else {
            // Refused (a field editor mid-edit, in theory): fall back to the
            // blunt set rather than leaving the editor out of step.
            string = text
            rehighlight()
            return
        }
        textStorage?.replaceCharacters(in: range, with: replacement)
        didChangeText()  // -> the delegate's textDidChange: binding + restyle
        let target = ComposeMarkers.caretAfterEdit(old: old, new: text, caret: caret)
        setSelectedRange(NSRange(location: target, length: 0))
        scrollRangeToVisible(NSRange(location: target, length: 0))
    }

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
        guard hasFiles(sender) else { return super.draggingEntered(sender) }
        onDropHover?(true)
        return .copy
    }

    override func draggingUpdated(_ sender: NSDraggingInfo) -> NSDragOperation {
        hasFiles(sender) ? .copy : super.draggingUpdated(sender)
    }

    override func draggingExited(_ sender: NSDraggingInfo?) {
        onDropHover?(false)
        super.draggingExited(sender)
    }

    override func performDragOperation(_ sender: NSDraggingInfo) -> Bool {
        onDropHover?(false)
        guard let onDropFiles, hasFiles(sender) else {
            return super.performDragOperation(sender)
        }
        let urls = ComposeDrop.fileURLs(on: sender.draggingPasteboard)
        let point = convert(sender.draggingLocation, from: nil)
        onDropFiles(urls, characterIndexForInsertion(at: point))
        return true
    }

    /// ⌘V with a file copied in the Finder attaches that file; ⌘V with a
    /// picture on the clipboard attaches it. A FILE WINS OVER TEXT: the Finder
    /// puts the file's NAME on the pasteboard as a string beside the url, and
    /// a copied text selection never carries a file url, so the url is the
    /// tell. A picture beside text is a text paste (a copied web selection) —
    /// see `ComposeDrop.imagePNG`.
    override func paste(_ sender: Any?) {
        let pasteboard = NSPasteboard.general
        let files = onDropFiles == nil ? [] : ComposeDrop.fileURLs(on: pasteboard)
        let png = files.isEmpty && onPasteImage != nil ? ComposeDrop.imagePNG(on: pasteboard) : nil
        guard !files.isEmpty || png != nil else {
            super.paste(sender)
            return
        }
        // A paste REPLACES the selection, as every paste does: the selected
        // words go through the text system (undoable) and the file lands
        // where they were.
        let selection = selectedRange()
        if selection.length > 0 { insertText("", replacementRange: selection) }
        let caret = selectedRange().location
        if !files.isEmpty, let onDropFiles {
            onDropFiles(files, caret)
        } else if let png, let onPasteImage {
            onPasteImage(png, caret)
        }
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
