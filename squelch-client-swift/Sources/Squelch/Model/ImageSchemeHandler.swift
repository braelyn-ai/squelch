// The `squelch-img:` responder — the only thing that can answer a rewritten
// email image reference (see ImageProxy).
//
// It is DELIBERATELY NOT A GENERIC FETCH PROXY. The only input it accepts is a
// `u` query item that parses as http or https AND carries this launch's
// signature; every other shape is refused before ImageStore is asked for
// anything. Page script cannot run (the frames disable it), so nothing in a
// message can mint a request at runtime — but a message CAN ship one in static
// markup, because the sanitizer scheme-filters href/src and leaves a
// `url(squelch-img://…)` in a <style> block alone. The signature is what
// separates a reference our own rewrite produced from one the mail wrote
// itself; see ImageProxy.
//
// The load-on-demand gate is upstream, in the CSP, and stays there: when the
// reader has not opted in, `img-src` is `data:` alone, so the `squelch-img:`
// request is blocked by the document and this handler is never called. Everything
// that reaches here is a request the CSP already permitted.
//
// LIFETIME. WebKit calls `start`/`stop` on the main thread and HARD-CRASHES the
// app if any method is called on a task after its `stop` — the task object is
// still alive and its selectors still exist, so there is no way to detect the
// mistake at the call site. Our answer is asynchronous by nature (disk, then
// possibly network), so a stopped task is a routine occurrence: closing a
// message, or reloading it on the images opt-in, tears down loads mid-flight.
// Hence the live set: a task identity is added on start, removed on stop or
// completion, and checked after every suspension before anything is sent.

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
    /// An ObjectIdentifier is an ADDRESS, so it only identifies anything while
    /// the object is alive — and what keeps this one alive is the escaping Task
    /// in `start`, which captures `urlSchemeTask` strongly for exactly as long
    /// as its key sits in this set. That capture is LOAD-BEARING: weaken it (or
    /// keep the identifier without the object) and a deallocated task's address
    /// can be handed to a new one, whose `stop` would then clear a live entry —
    /// and answering a stopped task hard-crashes the app.
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
            // above still holds for all of them: WebKit cannot deliver `stop`
            // in the middle of a synchronous main-thread run.
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

    /// A miss, a refused URL or a dead host all end the same way: fail the task
    /// and let the frame draw its broken-image glyph. Substituting a placeholder
    /// would be a lie about what the mail contains.
    private func fail(_ urlSchemeTask: any WKURLSchemeTask, _ key: ObjectIdentifier) {
        guard live.contains(key) else { return }
        live.remove(key)
        urlSchemeTask.didFailWithError(URLError(.resourceUnavailable))
    }
}
