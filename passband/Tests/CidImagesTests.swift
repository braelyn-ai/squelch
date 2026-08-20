// Inline `cid:` images: what the rewrite resolves, what it drops, and what it
// refuses to be talked into.
//
// The pass has two failure directions and they are not symmetric. A reference it
// fails to resolve costs a picture the reader could have seen; a reference it
// resolves TOO EAGERLY hands a message bytes it was never sent — which is why the
// signature, the inline gate and the neutered scheme token are each asserted here
// rather than read off the source.
//
// The other half is the double render. The strip stops tiling exactly the parts
// the body places, so `referencedAttachmentIDs` and `rewrite` have to agree about
// which ones those are, on every input. Two answers from one walk, checked
// against each other.

import Foundation

@main
@MainActor
struct CidImagesTests {
    static var failures = 0
    static var checks = 0

    static func main() {
        bothQuotingsResolve()
        entityEncodedSrcResolves()
        angleBracketsAndCaseAreReconciled()
        percentEncodedReferencesResolve()
        anUnmatchedReferenceDropsTheWholeTag()
        theInlineGateDropsWhatItRefuses()
        firstMatchWinsAndTheGateIsAppliedToIt()
        remoteImagesAreUntouched()
        mailAuthoredSchemeTokensAreNeutered()
        aForgedSignatureIsRefused()
        mintedUrlsRoundTrip()
        referencedIDsAgreeWithTheRewrite()
        theTrackerPassRunsBeforeBothAnswers()
        theCSPNamesTheCidSchemeInBothOptStates()

        if failures > 0 {
            print("FAILED: \(failures) of \(checks) checks")
            exit(1)
        }
        print("ok: \(checks) checks passed")
    }

    // MARK: - what resolves

    private static func bothQuotingsResolve() {
        for tag in [
            "<img src=\"cid:logo@passband\">",
            "<img src='cid:logo@passband'>",
            "<img alt=\"the logo\" src=\"cid:logo@passband\" width=\"200\">",
        ] {
            let out = CidImages.rewrite(html: tag, attachments: [photo])
            equal(minted(out) != nil, true, "resolves: \(tag)")
            equal(out.contains("cid:logo@passband"), false, "original reference is gone: \(tag)")
            // Everything OUTSIDE the src is spliced through untouched.
            equal(out.hasPrefix("<img "), true, "tag survives: \(tag)")
        }
        let attrs = CidImages.rewrite(
            html: "<img alt=\"the logo\" src=\"cid:logo@passband\" width=\"200\">",
            attachments: [photo])
        equal(attrs.contains("alt=\"the logo\""), true, "other attributes are preserved")
        equal(attrs.contains("width=\"200\""), true, "and so are the ones after src")
    }

    private static func entityEncodedSrcResolves() {
        // A serialized attribute value: the `&` in a Content-ID arrives escaped,
        // and the reference names the part only after it is decoded.
        let out = CidImages.rewrite(
            html: "<img src=\"cid:a&amp;b@host\">", attachments: [part(cid: "a&b@host")])
        equal(minted(out) != nil, true, "entity-encoded reference resolves")
    }

    private static func angleBracketsAndCaseAreReconciled() {
        // The header spelling on the stored side, the bare one in the body.
        equal(
            minted(CidImages.rewrite(html: bare, attachments: [part(cid: "<logo@passband>")]))
                != nil, true, "a bracketed stored content-id matches a bare reference")
        // And the other way round. Spelled as the sanitizer would hand it over:
        // a raw `>` inside an attribute value is not something ammonia emits,
        // and the walker takes the first one it sees as the end of the tag.
        equal(
            minted(
                CidImages.rewrite(
                    html: "<img src=\"cid:&lt;logo@passband&gt;\">", attachments: [photo]))
                != nil, true, "a bracketed reference matches a bare stored content-id")
        // Case folding is the fallback, not the first rule, but it must work.
        equal(
            minted(CidImages.rewrite(html: bare, attachments: [part(cid: "LOGO@PASSBAND")]))
                != nil, true, "a case-folded content-id still matches")
        equal(
            minted(CidImages.rewrite(html: "<img src=\"CID:logo@passband\">", attachments: [photo]))
                != nil, true, "the scheme itself is case-insensitive")
    }

    private static func percentEncodedReferencesResolve() {
        // RFC 2392 makes the reference a URI, so a Content-ID with a space in it
        // is spelled `%20` there and stored plain.
        let out = CidImages.rewrite(
            html: "<img src=\"cid:part%20one@host\">", attachments: [part(cid: "part one@host")])
        equal(minted(out) != nil, true, "percent-encoded reference resolves")
    }

    // MARK: - what drops

    private static func anUnmatchedReferenceDropsTheWholeTag() {
        // The pre-migration case, and the whole reason the drop is the default:
        // mail synced before the daemon recorded Content-IDs matches nothing.
        let out = CidImages.rewrite(
            html: "<p>before</p><img src=\"cid:nobody@here\"><p>after</p>",
            attachments: [photo])
        equal(out, "<p>before</p><p>after</p>", "an unmatched reference leaves no tag behind")

        equal(
            CidImages.rewrite(html: bare, attachments: []), "",
            "a body with no parts at all drops its references too")
    }

    private static func theInlineGateDropsWhatItRefuses() {
        let refused: [(String, Attachment)] = [
            ("a pdf is not an inline image", part(mime: "application/pdf")),
            ("svg is never rendered", part(mime: "image/svg+xml")),
            ("unstored bytes cannot be served", part(downloadable: false)),
            ("past the inline cap", part(size: AttachmentKinds.inlineMaxBytes + 1)),
        ]
        for (label, att) in refused {
            equal(CidImages.rewrite(html: bare, attachments: [att]), "", label)
            equal(
                CidImages.referencedAttachmentIDs(html: bare, attachments: [att]), [],
                "\(label) — and it is not reported as in-body")
        }
        // The boundary itself is inside the gate.
        equal(
            minted(
                CidImages.rewrite(
                    html: bare, attachments: [part(size: AttachmentKinds.inlineMaxBytes)])) != nil,
            true, "exactly at the cap still renders")
    }

    private static func firstMatchWinsAndTheGateIsAppliedToIt() {
        // Two parts under one Content-ID is malformed mail. The first is the
        // answer, and if IT fails the gate the tag goes — picking the second
        // would be guessing which part the sender meant.
        let out = CidImages.rewrite(
            html: bare,
            attachments: [
                part(id: 11, mime: "application/pdf"), part(id: 12),
            ])
        equal(out, "", "the first part under a shared content-id decides, gate included")

        let ordered = CidImages.referencedAttachmentIDs(
            html: bare, attachments: [part(id: 12), part(id: 13)])
        equal(ordered, [12], "and when it passes, the first is the one that renders")
    }

    private static func remoteImagesAreUntouched() {
        // Not a cid reference: this pass has no opinion, and ImageProxy runs
        // after it. A body whose art is all remote must come out byte-identical.
        let source = "<img src=\"https://cdn.example.com/hero.png\"><img src=\"data:image/gif,x\">"
        equal(CidImages.rewrite(html: source, attachments: [photo]), source, "remote art is kept")
        equal(
            CidImages.referencedAttachmentIDs(html: source, attachments: [photo]), [],
            "and nothing is claimed as in-body")
    }

    // MARK: - what must not be talked into

    private static func mailAuthoredSchemeTokensAreNeutered() {
        // The mail spells our own scheme. The signature check already refuses it;
        // neutering the token demotes it to one the CSP never dispatches.
        let source = "<style>.a{background:url(passband-cid://local/deadbeef?a=91&n=x)}</style>"
        let out = CidImages.rewrite(html: source, attachments: [photo])
        equal(out.contains("passband-cid://"), false, "a hand-written scheme token does not survive")
        equal(out.contains("passband-cid-blocked:"), true, "it is demoted, not deleted")
    }

    private static func aForgedSignatureIsRefused() {
        let honest = minted(CidImages.rewrite(html: bare, attachments: [photo]))
        equal(honest != nil, true, "the reference minted at all")
        guard let honest, let url = URL(string: honest) else { return }
        equal(CidProxy.attachment(from: url)?.id, photo.id, "our own url parses")

        // The whole point: a body cannot ask for a part by writing the url out.
        let forgeries = [
            "passband-cid://local/deadbeef?a=91&n=x",
            "passband-cid://local/?a=91&n=x",
            // This launch's signature over a DIFFERENT attachment, pointed at
            // another id — the scoping the signature exists for.
            honest.replacingOccurrences(of: "?a=\(photo.id)", with: "?a=91"),
            // Same signature, different filename: the name decides what the
            // staged file is called, so it is signed too.
            honest + "x",
        ]
        for forged in forgeries {
            let parsed = URL(string: forged).flatMap(CidProxy.attachment(from:))
            equal(parsed?.id, nil, "refused: \(forged)")
        }
        // A url minted for another scheme is not ours either.
        equal(
            URL(string: "passband-img://local/x?u=https://h/a.png")
                .flatMap(CidProxy.attachment(from:))?.id, nil, "the proxy scheme is not this one")
    }

    private static func mintedUrlsRoundTrip() {
        // A filename with everything that has to survive percent-encoding: a
        // space, an ampersand (which would otherwise end the query item), and a
        // non-ASCII letter.
        let awkward = part(filename: "réunion & co #1.jpeg")
        guard let url = URL(string: CidProxy.url(for: awkward)) else {
            equal(false, true, "the minted url parses")
            return
        }
        let parsed = CidProxy.attachment(from: url)
        equal(parsed?.id, awkward.id, "the id survives the round trip")
        equal(parsed?.filename, awkward.filename, "and so does the filename, exactly")

        // An empty name would stage as nothing; the mint substitutes one.
        let nameless = URL(string: CidProxy.url(for: part(filename: "")))
            .flatMap(CidProxy.attachment(from:))
        equal(nameless?.filename, "attachment", "a nameless part still names its file")
    }

    // MARK: - the two answers agree

    private static func referencedIDsAgreeWithTheRewrite() {
        let body = """
            <p>see attached</p>\
            <img src="cid:logo@passband">\
            <img src='cid:shot@passband'>\
            <img src="cid:missing@passband">\
            <img src="https://cdn.example.com/hero.png">
            """
        let parts = [photo, part(id: 12, cid: "shot@passband"), part(id: 13, cid: "other@host")]
        let ids = CidImages.referencedAttachmentIDs(html: body, attachments: parts)
        equal(ids, [photo.id, 12], "only the two that resolved are reported")

        // The agreement is the invariant: one minted url per reported id, and
        // one dropped tag per reference that is not.
        let out = CidImages.rewrite(html: body, attachments: parts)
        equal(
            out.matches(of: /passband-cid:\/\//).count, ids.count,
            "the rewrite mints exactly what the id set claims")
        equal(out.contains("cid:missing@passband"), false, "and the unmatched one is gone")
        equal(out.contains("https://cdn.example.com/hero.png"), true, "remote art still stands")
        // The part nobody referenced is nobody's business here.
        equal(ids.contains(13), false, "an unreferenced part is not in-body")
    }

    private static func theTrackerPassRunsBeforeBothAnswers() {
        // A `cid:` image the sender hid, which the tracker pass drops before the
        // rewrite is ever handed the body — its tests read a size and a style,
        // not a scheme. Both callers must therefore ask about the STRIPPED html:
        // asking about the raw one reports an id the body does not draw, and the
        // attachment strip then withholds the tile of a photo that renders
        // nowhere. MessageCard.inBodyImages strips for exactly this reason.
        let hidden = "<img src=\"cid:logo@passband\" style=\"display:none\">"
        let tiny = "<img src=\"cid:logo@passband\" width=\"1\" height=\"1\">"
        for body in [hidden, tiny] {
            equal(
                CidImages.referencedAttachmentIDs(html: body, attachments: [photo]), [photo.id],
                "the raw body claims the part is in-body: \(body)")
            let stripped = Trackers.strip(body).html
            equal(stripped.contains("cid:"), false, "the tracker pass took the tag: \(body)")
            equal(
                CidImages.referencedAttachmentIDs(html: stripped, attachments: [photo]), [],
                "so the stripped body claims nothing: \(body)")
            equal(
                minted(CidImages.rewrite(html: stripped, attachments: [photo])), nil,
                "and there is nothing to mint: \(body)")
        }
        // A visible photo is untouched by that pass, so the two agree there.
        let visible = "<img src=\"cid:logo@passband\" width=\"600\" height=\"400\">"
        equal(
            CidImages.referencedAttachmentIDs(
                html: Trackers.strip(visible).html, attachments: [photo]), [photo.id],
            "a photo the sender meant to be seen survives both passes")
    }

    // MARK: - the policy

    private static func theCSPNamesTheCidSchemeInBothOptStates() {
        // Attachment bytes are this message's own, fetched from the user's own
        // daemon, so the remote-image opt-in has nothing to say about them.
        for allowRemote in [true, false] {
            let policy = MailCSP.policy(allowRemote: allowRemote)
            equal(
                policy.contains("passband-cid:"), true,
                "img-src names the cid scheme (allowRemote: \(allowRemote))")
            equal(policy.contains("default-src 'none'"), true, "the floor is unchanged")
            equal(
                policy.contains("http:") || policy.contains("https:"), false,
                "and the network is never named directly")
        }
        equal(
            MailCSP.policy(allowRemote: false).contains("passband-img:"), false,
            "the proxy scheme stays behind the opt-in")
        equal(
            MailCSP.policy(allowRemote: true).contains("passband-img:"), true,
            "and appears once it is taken")
    }

    // MARK: - helpers

    /// A body carrying one bare reference to `photo`.
    private static let bare = "<img src=\"cid:logo@passband\">"

    /// The part that reference names: a phone photo, comfortably inline.
    private static let photo = part()

    private static func part(
        id: Int = 11, cid: String? = "logo@passband", mime: String = "image/jpeg",
        size: Int = 1_582_543, downloadable: Bool = true, filename: String = "IMG_3480.jpeg"
    ) -> Attachment {
        Attachment(
            id: id, filename: filename, mime: mime, size: size, downloadable: downloadable,
            content_id: cid)
    }

    /// The minted url in a rewritten body, if there is one.
    private static func minted(_ html: String) -> String? {
        html.firstMatch(of: /passband-cid:\/\/[^"']+/).map { String($0.0) }
    }

    private static func equal<T: Equatable>(
        _ got: T, _ want: T, _ label: String, line: Int = #line
    ) {
        checks += 1
        guard got != want else { return }
        failures += 1
        print("line \(line): \(label)\n  got:  \(got)\n  want: \(want)")
    }
}
