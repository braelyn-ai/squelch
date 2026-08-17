// THE MAC'S PREVIEW IS THE SYSTEM'S. Space-bar Quick Look — the same panel
// Finder opens — instead of a card this app draws. It zooms out of the card that
// was clicked, arrows between the rest of the message's attachments, and brings
// its own zoom, rotate, share and "Open with Preview". What it replaced was two
// hand-rolled sheets, a PDFView card and an ImageIO card, each with its own
// scrim, its own clamped 820x520 frame and its own Esc, between them able to
// render exactly two file types.
//
// THREE THINGS ARE LOAD-BEARING:
//
//   1. THE RESPONDER CHAIN IS THE API. QLPreviewPanel is a process-wide
//      singleton that asks the FIRST RESPONDER who is driving it — there is no
//      delegate you simply hand it. SwiftUI has no responder to offer, so the
//      strip plants a view behind its cards that draws nothing, swallows no
//      clicks, and becomes first responder on the way in. That is also what
//      settles which of a thread's many strips owns the panel: the one that was
//      clicked.
//   2. QUICK LOOK READS FILES, so bytes are fetched and staged on disk per item,
//      and lazily — opening one photo must not download the other eleven. An
//      item whose fetch is still in flight has no URL yet and previews blank;
//      `refreshCurrentPreviewItem()` fills it in when the bytes land.
//   3. THE PANEL IS ONE OF OUR WINDOWS, so the app's global key monitor sees its
//      Escape before it does and would eat it. KeyMonitor stands down for the
//      panel explicitly — see Keys/KeyDispatch.swift.

#if os(macOS)

    import AppKit
    import QuickLookUI
    import SwiftUI

    /// One attachment as Quick Look sees it. A CLASS, because the URL arrives
    /// after the panel is already showing the item: the panel holds this object
    /// and re-reads `previewItemURL` when told to refresh.
    final class QuickLookItem: NSObject, QLPreviewItem {
        let attachment: Attachment
        var staged: StagedAttachment?

        init(_ attachment: Attachment) { self.attachment = attachment }

        var previewItemURL: URL? { staged?.url }
        /// The mail filename, not the staging directory's: the human named this
        /// file, and the UUID above it is our bookkeeping.
        var previewItemTitle: String? { attachment.filename }
    }

    /// The strip's foothold in the responder chain, and the panel's data source
    /// once it has one.
    final class QuickLookHostView: NSView {
        /// Everything in this strip a click can open, in display order, so the
        /// panel's arrows walk the message's attachments the way the eye does.
        private(set) var items: [QuickLookItem] = []
        /// Where each attachment's clickable rect sits IN THIS VIEW'S OWN
        /// coordinates. SwiftUI measures them in a named space pinned to this
        /// view's frame, and `isFlipped` is what lets them be used as measured.
        var sourceFrames: [Int: CGRect] = [:]
        /// How a failed fetch gets said out loud — the toast belongs to the
        /// store, which is a SwiftUI thing this view has no handle on.
        var onError: ((String) -> Void)?

        private var pendingIndex = 0
        private var fetching: Set<Int> = []

        // Top-left origin, matching SwiftUI, so a measured rect needs no flip.
        override var isFlipped: Bool { true }
        override var acceptsFirstResponder: Bool { true }
        // It sits BEHIND the cards. A background view that answered hit tests
        // would swallow the clicks of every card it covers.
        override func hitTest(_ point: NSPoint) -> NSView? { nil }

        // MARK: - contents

        /// Rebuild the item list only when the attachments actually changed.
        /// Rebuilding on every re-render would hand the panel fresh objects and
        /// drop the URLs already staged into the old ones.
        func setAttachments(_ attachments: [Attachment]) {
            guard attachments.map(\.id) != items.map(\.attachment.id) else { return }
            items = attachments.map(QuickLookItem.init)
        }

        // MARK: - opening

        func open(_ attachment: Attachment) {
            guard let index = items.firstIndex(where: { $0.attachment.id == attachment.id })
            else { return }
            pendingIndex = index
            // First, because the panel reads the responder chain the moment it is
            // asked to appear.
            window?.makeFirstResponder(self)

            guard let panel = QLPreviewPanel.shared() else { return }
            if panel.isVisible {
                // Already up on some other strip's attachment: hand it over
                // rather than opening a second one. There is only ever one panel.
                panel.updateController()
                panel.reloadData()
                panel.currentPreviewItemIndex = index
            } else {
                // Calls back into beginPreviewPanelControl, which is where the
                // data source and the starting index get set.
                panel.makeKeyAndOrderFront(nil)
            }
            stage(index)
        }

        // The three below come from an informal NSObject category, so Swift sees
        // them as nonisolated even though Quick Look only ever calls them from
        // the main thread — it is driving AppKit windows from AppKit's own
        // machinery. `assumeIsolated` is that fact written down rather than a
        // hop, which would land after the panel has already read what it asked.

        override func acceptsPreviewPanelControl(_ panel: QLPreviewPanel!) -> Bool { true }

        override func beginPreviewPanelControl(_ panel: QLPreviewPanel!) {
            MainActor.assumeIsolated {
                panel.dataSource = self
                panel.delegate = self
                panel.reloadData()
                panel.currentPreviewItemIndex = pendingIndex
            }
        }

        override func endPreviewPanelControl(_ panel: QLPreviewPanel!) {
            MainActor.assumeIsolated {
                panel.dataSource = nil
                panel.delegate = nil
                releaseStagedBytes()
            }
        }

        // MARK: - staging

        /// Fetch and stage one item's bytes if they are not already on disk.
        private func stage(_ index: Int) {
            guard items.indices.contains(index) else { return }
            let item = items[index]
            let id = item.attachment.id
            guard item.staged == nil, !fetching.contains(id) else { return }
            fetching.insert(id)
            Task { @MainActor in
                defer { fetching.remove(id) }
                do {
                    let fetched = try await APIClient.shared.fetchAttachment(
                        id, fallbackName: item.attachment.filename)
                    item.staged = try StagedAttachment.stage(
                        id: id, bytes: fetched.bytes, filename: fetched.filename)
                } catch {
                    onError?(errText(error, "preview failed"))
                    return
                }
                // Only nudge the panel if it is still ours and still looking at
                // this item — an arrow key during the fetch means the human has
                // moved on and a refresh would yank them back.
                guard QLPreviewPanel.sharedPreviewPanelExists(),
                    let panel = QLPreviewPanel.shared(),
                    panel.isVisible, panel.dataSource === self,
                    panel.currentPreviewItemIndex == index
                else { return }
                panel.refreshCurrentPreviewItem()
            }
        }

        /// The staged files go when the panel does. The delay is for the panel's
        /// own "Open with Preview": that hands the URL to another app and closes,
        /// so deleting on the spot would pull the file out from under the app
        /// just asked to show it. Nothing survives the session either way —
        /// StagedAttachment.purgeRoot() sweeps at launch.
        private func releaseStagedBytes() {
            let staged = items.compactMap(\.staged)
            for item in items { item.staged = nil }
            guard !staged.isEmpty else { return }
            Task.detached(priority: .background) {
                try? await Task.sleep(for: .seconds(30))
                for file in staged { file.cleanUp() }
            }
        }

        // MARK: - teardown

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            if window == nil { detachFromPanel() }
        }

        /// Let go of the panel. `dataSource` and `delegate` are UNOWNED, so a
        /// host that goes away while the panel is still pointing at it leaves the
        /// panel holding a dead object.
        func detachFromPanel() {
            guard QLPreviewPanel.sharedPreviewPanelExists(),
                let panel = QLPreviewPanel.shared(),
                panel.dataSource === self
            else { return }
            panel.dataSource = nil
            panel.delegate = nil
            if panel.isVisible { panel.orderOut(nil) }
            releaseStagedBytes()
        }
    }

    // MARK: - data source

    extension QuickLookHostView: @preconcurrency QLPreviewPanelDataSource {
        func numberOfPreviewItems(in panel: QLPreviewPanel!) -> Int { items.count }

        func previewPanel(_ panel: QLPreviewPanel!, previewItemAt index: Int) -> QLPreviewItem! {
            guard items.indices.contains(index) else { return nil }
            // The panel asks for what it is about to show, and for the neighbours
            // it would show next. Staging exactly that window is what keeps
            // opening one photo from downloading the other eleven while still
            // making the arrow keys feel instant.
            if abs(index - panel.currentPreviewItemIndex) <= 1 { stage(index) }
            return items[index]
        }
    }

    // MARK: - delegate

    extension QuickLookHostView: @preconcurrency QLPreviewPanelDelegate {
        /// Where the panel zooms out of. `.zero` is the graceful answer for an
        /// item whose card we cannot place — the panel fades in instead.
        func previewPanel(
            _ panel: QLPreviewPanel!, sourceFrameOnScreenFor item: QLPreviewItem!
        ) -> NSRect {
            guard let item = item as? QuickLookItem,
                let frame = sourceFrames[item.attachment.id],
                let window
            else { return .zero }
            return window.convertToScreen(convert(frame, to: nil))
        }

        /// The art the zoom carries. The thumbnail cache already holds exactly
        /// what the card is painting, so the picture flies rather than a blank.
        func previewPanel(
            _ panel: QLPreviewPanel!, transitionImageFor item: QLPreviewItem!,
            contentRect: UnsafeMutablePointer<NSRect>!
        ) -> Any! {
            guard let item = item as? QuickLookItem else { return nil }
            let id = item.attachment.id
            let tile = AttachmentThumbs.shared.cachedInline(id) ?? AttachmentThumbs.shared.cached(id)
            guard case .art(let image)? = tile else { return nil }
            return image
        }
    }

    // MARK: - SwiftUI

    /// The handle SwiftUI holds onto: a strip keeps one in `@State` and calls
    /// `open` from a tap, which is the only thing it ever needs from AppKit.
    @MainActor
    final class QuickLookLauncher {
        fileprivate weak var host: QuickLookHostView?

        nonisolated init() {}

        func open(_ attachment: Attachment) { host?.open(attachment) }
    }

    /// Plants the host view in the strip's background. Zero chrome by design:
    /// everything visible belongs to the panel.
    struct QuickLookHost: NSViewRepresentable {
        let attachments: [Attachment]
        let sourceFrames: [Int: CGRect]
        let launcher: QuickLookLauncher
        let onError: (String) -> Void

        func makeNSView(context: Context) -> QuickLookHostView {
            let view = QuickLookHostView()
            configure(view)
            return view
        }

        func updateNSView(_ view: QuickLookHostView, context: Context) {
            configure(view)
        }

        private func configure(_ view: QuickLookHostView) {
            launcher.host = view
            view.setAttachments(attachments)
            view.sourceFrames = sourceFrames
            view.onError = onError
        }

        static func dismantleNSView(_ view: QuickLookHostView, coordinator: Coordinator) {
            MainActor.assumeIsolated { view.detachFromPanel() }
        }
    }

#endif  // os(macOS)
