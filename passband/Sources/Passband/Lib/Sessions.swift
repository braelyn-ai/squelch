// ONE place that builds URLSessions, so "what this app is willing to leak on the
// wire" is a list of named knobs rather than six hand-copied configurations.
// Every session here is ephemeral: nothing this app fetches — a token-bearing
// request, an image an email chose — has any business surviving the launch.

import Foundation

enum Sessions {
    /// Cookie posture. An ephemeral session already keeps its jar in memory and
    /// drops it at exit; these say whether one is used at all.
    enum Cookies {
        /// The session's own in-memory jar, sent and accepted (URLSession's own default).
        case sessionJar
        /// Kept, but never SENT — nothing we hold identifies the reader on the wire.
        case neverSent
        /// No jar at all: never sent, never accepted, nowhere to keep one.
        case off
    }

    /// Build one ephemeral session. Every parameter defaults to URLSession's own
    /// posture, so a call site names ONLY the knobs it actually turns.
    ///
    /// - Parameters:
    ///   - timeout: idle gap allowed between bytes.
    ///   - resource: WALL CLOCK for the whole transfer, which `timeout` is NOT —
    ///     a host dribbling a byte a second never idles out. Unset, this defaults
    ///     to seven days.
    ///   - urlCache: false drops the response cache entirely, for callers that
    ///     ARE the cache and would otherwise hold a second copy of the bytes.
    ///   - emptyHeaders: send no default additional headers.
    ///   - waitsForConnectivity: false fails a connect to a dead host immediately
    ///     instead of parking it for the resource timeout.
    static func ephemeral(
        timeout: TimeInterval,
        resource: TimeInterval? = nil,
        cookies: Cookies = .sessionJar,
        urlCache: Bool = true,
        cachePolicy: URLRequest.CachePolicy = .useProtocolCachePolicy,
        emptyHeaders: Bool = false,
        waitsForConnectivity: Bool = false
    ) -> URLSession {
        let cfg = URLSessionConfiguration.ephemeral
        cfg.timeoutIntervalForRequest = timeout
        if let resource { cfg.timeoutIntervalForResource = resource }
        switch cookies {
        case .sessionJar:
            break
        case .neverSent:
            cfg.httpShouldSetCookies = false
        case .off:
            cfg.httpShouldSetCookies = false
            cfg.httpCookieAcceptPolicy = .never
            cfg.httpCookieStorage = nil
        }
        if !urlCache { cfg.urlCache = nil }
        cfg.requestCachePolicy = cachePolicy
        if emptyHeaders { cfg.httpAdditionalHeaders = [:] }
        cfg.waitsForConnectivity = waitsForConnectivity
        return URLSession(configuration: cfg)
    }
}

/// Redirect policy as a per-task delegate: re-guards every hop against the
/// schemes the caller is willing to land on, so a 302 cannot walk a fetch onto
/// `file:` or any other scheme this process can reach. An empty `allow` follows
/// nothing at all, which surfaces the 3xx to the caller as a non-200.
final class SchemePinned: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
    private let allow: Set<String>

    init(allow: Set<String>) { self.allow = allow }

    func urlSession(
        _ session: URLSession, task: URLSessionTask,
        willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest,
        completionHandler: @escaping (URLRequest?) -> Void
    ) {
        guard let scheme = request.url?.scheme?.lowercased(), allow.contains(scheme) else {
            completionHandler(nil)
            return
        }
        completionHandler(request)
    }
}
