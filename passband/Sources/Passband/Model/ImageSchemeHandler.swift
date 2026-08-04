// The `passband-img:` responder — the only thing that can answer a rewritten email
// image reference (ImageProxy mints them). NOT a generic fetch proxy: the sole
// accepted input is a `u` that parses as http(s) AND carries this launch's
// signature, which is what separates our own rewrite from a `url(passband-img://…)`
// the mail wrote itself in a kept <style> block. The load-on-demand gate is
// upstream in the CSP, so everything reaching here was already permitted by the
// document. See docs/SECURITY.md §3.
//
// LIFETIME: answering a task after WebKit's `stop` HARD-CRASHES the app and cannot
// be detected at the call site, and our answer is async (disk, maybe network), so a
// stopped task is routine — hence the live set, checked after every suspension.

import Foundation
import WebKit

@MainActor
final class ImageSchemeHandler: NSObject, WKURLSchemeHandler {
    /// One handler for every email configuration. A WKURLSchemeHandler is
    /// allowed to be shared, and sharing keeps the live set in one place.
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

        guard let url = urlSchemeTask.request.url,
            let original = ImageProxy.original(from: url)
        else {
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
            // No suspension point separates these three, so the liveness check
            // above holds for all of them: WebKit cannot deliver `stop` in the
            // middle of a synchronous main-thread run.
            let response = URLResponse(
                url: url, mimeType: mime, expectedContentLength: bytes.count,
                textEncodingName: nil)
            urlSchemeTask.didReceive(response)
            urlSchemeTask.didReceive(bytes)
            urlSchemeTask.didFinish()
            self.live.remove(key)
        }
    }

    func webView(_ webView: WKWebView, stop urlSchemeTask: any WKURLSchemeTask) {
        live.remove(ObjectIdentifier(urlSchemeTask))
    }

    /// A miss, a refused URL and a dead host all end the same way: fail the task and
    /// let the frame draw its broken-image glyph, rather than substituting a
    /// placeholder that would lie about what the mail contains.
    private func fail(_ urlSchemeTask: any WKURLSchemeTask, _ key: ObjectIdentifier) {
        guard live.contains(key) else { return }
        live.remove(key)
        urlSchemeTask.didFailWithError(URLError(.resourceUnavailable))
    }
}
