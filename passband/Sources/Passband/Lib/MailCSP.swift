// THE READING FRAME'S CONTENT-SECURITY-POLICY — layer 3 of five (see
// docs/SECURITY.md), spelled here rather than inline in the document builder so
// the one string the sandbox rests on can be asserted without standing a
// WKWebView up. Nothing else in EmailWebCore is testable that way, and this is
// the line of it that matters.

enum MailCSP {
    /// The whole policy. `default-src 'none'` is the floor: no script, no frame,
    /// no font, no connect, whatever the mail asks for. Inline styles are the one
    /// exception, because email IS inline styles.
    static func policy(allowRemote: Bool) -> String {
        "default-src 'none'; style-src 'unsafe-inline'; img-src \(imgSrc(allowRemote: allowRemote))"
    }

    /// The one resource a body may load, and never as itself:
    ///
    /// `passband-img:` and NOT `http: https:` — every remote image was rewritten
    /// to the proxy scheme (ImageProxy), so the network is reachable only through
    /// ImageSchemeHandler and anything the rewrite missed fails closed. Its
    /// presence is ALSO the whole load-on-demand gate: an un-opted message has no
    /// `passband-img:` in its policy, so the document refuses the request before
    /// the handler is reached and nothing downstream re-checks.
    ///
    /// `passband-cid:` is allowed in BOTH states, deliberately. A cid reference
    /// resolves to a part of THIS message, already synced, fetched from the
    /// user's own daemon over the authenticated door — there is no third party to
    /// learn anything from it, so the remote-image opt-in has nothing to say
    /// about it. What scopes those urls is the per-launch signature CidProxy
    /// mints them with, not the policy.
    ///
    /// `data:` stays for inline art.
    static func imgSrc(allowRemote: Bool) -> String {
        allowRemote
            ? "\(ImageProxy.scheme): \(CidProxy.scheme): data:"
            : "\(CidProxy.scheme): data:"
    }
}
