// The responder for the two schemes an email body is allowed to load images
// from, and the only thing that can answer either. NOT a generic fetch proxy: a
// url gets bytes only if it carries THIS launch's signature, which is what
// separates our own rewrite from a `url(passband-img://…)` the mail wrote itself
// in a kept <style> block.
//
//   passband-img: — remote art, minted by ImageProxy, fetched by ImageStore. The
//   load-on-demand gate is upstream in the CSP, so everything reaching here was
//   already permitted by the document.
//
//   passband-cid: — a part of THIS message, minted by CidProxy, fetched from the
//   authenticated attachment door (a bearer token cannot ride an <img src>).
//   Allowed by the CSP in both opt states, so unlike the proxy scheme the
//   signature is the whole gate.
//
// One class for both because the lifetime rule below is the hard part and must
// not be written twice. See docs/SECURITY.md §3.
//
// LIFETIME: answering a task after WebKit's `stop` HARD-CRASHES the app and cannot
// be detected at the call site, and our answer is async (disk, maybe network), so a
// stopped task is routine — hence the live set, checked after every suspension.

import Foundation
import WebKit

@MainActor
final class ImageSchemeHandler: NSObject, WKURLSchemeHandler {
    /// One handler for every email configuration and both schemes. A
    /// WKURLSchemeHandler is allowed to be shared, and sharing keeps the live set
    /// in one place.
    static let shared = ImageSchemeHandler()

    /// Tasks WebKit has started and not yet stopped. Main-thread only, which is
    /// where every WKURLSchemeHandler callback lands.
    ///
    /// An ObjectIdentifier is an ADDRESS, valid only while the object lives, and
    /// what keeps this one alive is the escaping Task in `start` capturing
    /// `urlSchemeTask` strongly for exactly as long as its key sits here. That
    /// capture is LOAD-BEARING: weaken it and a dead task's address can be reused
    /// by a new one, whose `stop` then clears a live entry — and answering a
    /// stopped task hard-crashes the app.
    private var live: Set<ObjectIdentifier> = []

    private override init() { super.init() }

    func webView(_ webView: WKWebView, start urlSchemeTask: any WKURLSchemeTask) {
        let key = ObjectIdentifier(urlSchemeTask)
        live.insert(key)

        guard let url = urlSchemeTask.request.url else {
            fail(urlSchemeTask, key)
            return
        }

        switch url.scheme?.lowercased() {
        case ImageProxy.scheme:
            guard let original = ImageProxy.original(from: url) else {
                fail(urlSchemeTask, key)
                return
            }
            Task { [weak self] in
                let hit = await ImageStore.shared.data(for: original)
                guard let self, self.live.contains(key) else { return }
                guard let (bytes, mime) = hit else {
                    self.fail(urlSchemeTask, key)
                    return
                }
                self.answer(urlSchemeTask, key, url: url, bytes: bytes, mime: mime)
            }
        case CidProxy.scheme:
            guard let part = CidProxy.attachment(from: url) else {
                fail(urlSchemeTask, key)
                return
            }
            Task { [weak self] in
                let hit = await Self.attachmentBytes(id: part.id, filename: part.filename)
                guard let self, self.live.contains(key) else { return }
                guard let (bytes, mime) = hit else {
                    self.fail(urlSchemeTask, key)
                    return
                }
                self.answer(urlSchemeTask, key, url: url, bytes: bytes, mime: mime)
            }
        default:
            fail(urlSchemeTask, key)
        }
    }

    func webView(_ webView: WKWebView, stop urlSchemeTask: any WKURLSchemeTask) {
        live.remove(ObjectIdentifier(urlSchemeTask))
    }

    /// Hand the bytes over. SYNCHRONOUS on purpose: no suspension point separates
    /// the three calls, so the caller's liveness check still holds for all of them
    /// — WebKit cannot deliver `stop` in the middle of a main-thread run.
    private func answer(
        _ urlSchemeTask: any WKURLSchemeTask, _ key: ObjectIdentifier, url: URL, bytes: Data,
        mime: String?
    ) {
        let response = URLResponse(
            url: url, mimeType: mime, expectedContentLength: bytes.count,
            textEncodingName: nil)
        urlSchemeTask.didReceive(response)
        urlSchemeTask.didReceive(bytes)
        urlSchemeTask.didFinish()
        live.remove(key)
    }

    /// A miss, a refused URL and a dead host all end the same way: fail the task and
    /// let the frame draw its broken-image glyph, rather than substituting a
    /// placeholder that would lie about what the mail contains.
    private func fail(_ urlSchemeTask: any WKURLSchemeTask, _ key: ObjectIdentifier) {
        guard live.contains(key) else { return }
        live.remove(key)
        urlSchemeTask.didFailWithError(URLError(.resourceUnavailable))
    }

    // MARK: - attachment bytes

    /// One part's bytes for a minted `passband-cid:` url: the staged file if the
    /// app already holds it, otherwise the authenticated door — and what comes
    /// back is staged, so the click that follows the picture opens instantly.
    /// (That used to fall out of the column rendering the same photo; a photo
    /// rendered IN THE BODY no longer goes through AttachmentThumbs at all.)
    private static func attachmentBytes(id: Int, filename: String) async -> (Data, String?)? {
        if let file = AttachmentFiles.shared.cached(id),
            let bytes = await staged(at: file.url)
        {
            // No mime: what was staged is a filename and bytes. WebKit sniffs,
            // and an <img> that sniffs to nothing it can decode draws nothing.
            return within(cap: bytes).map { ($0, nil) }
        }
        guard
            let fetched = try? await APIClient.shared.fetchAttachment(id, fallbackName: filename),
            let bytes = within(cap: fetched.bytes)
        else { return nil }
        AttachmentFiles.shared.keep(id: id, bytes: bytes, filename: fetched.filename)
        // The response's own Content-Type, but ONLY while it still says image:
        // the gate that minted this url read the mime out of the message JSON,
        // and nothing promises the byte door agrees with it.
        let mime = AttachmentKinds.isRenderableImage(fetched.mime) ? fetched.mime : nil
        return (bytes, mime)
    }

    /// Defense in depth against a part whose declared size lied: the rewrite
    /// already refused anything over the inline cap, and so does this.
    private static func within(cap bytes: Data) -> Data? {
        bytes.count <= AttachmentKinds.inlineMaxBytes ? bytes : nil
    }

    /// Read a staged file OFF the main actor — it is up to twelve megabytes, and
    /// every scheme-handler callback lands on the main thread.
    private nonisolated static func staged(at url: URL) async -> Data? {
        await Task.detached(priority: .userInitiated) {
            try? Data(contentsOf: url, options: .mappedIfSafe)
        }.value
    }
}
